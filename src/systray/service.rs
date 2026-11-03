//! `org.kde.StatusNotifierWatcher` 的 D-Bus 服务。
//!
//! 线程模型与通知一致：D-Bus 连接、item 属性查询、dbusmenu 菜单都在一个独立
//! 线程里；合成器通过 [`TrayHandle`] 发命令，服务把 [`Snapshot`] 推回 calloop
//! channel。渲染线程里没有 async 运行时，也不做任何阻塞调用。
//!
//! 与基线的对应关系：
//!
//! - `watcher.c` → 这里的 `Host` 接口 + [`Service`] 的注册/注销；
//! - `item.c` → [`Service::load_item`] / [`Service::load_pixmap`]；
//! - `menu.c` → [`Service::open_menu`] / [`Service::pick`]；
//! - `tray.c` 的绘制 → `ui::bar` 与 `ui::tray`。
//!
//! 失败一律非致命：没有会话总线、名字被别的宿主占用、item 不响应属性查询，
//! 都只记日志并继续，合成器照常运行。

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use zbus::{
    MatchRule,
    blocking::{Connection, MessageIterator, connection::Builder},
    message::{Header, Type},
    names::BusName,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use super::{
    DBUSMENU_IFACE, ItemView, MENU_ITEMS_MAX, MenuItem, SNI_HOST_NAME, SNI_HOST_PATH, SNI_IFACE,
    SNI_ITEM_PATH, Snapshot, format_label,
};
use crate::ui::icon::{self, Icon};

/// 主题图标按这个边长解码；bar 真正绘制时再缩到图标格（`fit` 允许缩放）。
const ICON_LOAD_SIZE: u32 = 48;
/// StatusNotifierItem 的 `IconPixmap` 里优先挑不小于该边长的位图（基线 22）。
const ICON_PREFERRED: i32 = 22;
/// 位图图标的存储上限，超过就缩到这个方框内，避免一个 item 占几十 MB。
const ICON_STORE_MAX: u32 = 256;

/// 合成器发给服务线程的命令。
#[derive(Debug)]
enum Command {
    /// item 调用了 `RegisterStatusNotifierItem`。
    Register { busname: String, busobj: String },
    /// 总线上的名字消失（`NameOwnerChanged` 的 new_owner 为空）。
    OwnerChanged { name: String, new_owner: String },
    /// item 发了 `NewIcon` 信号。
    NewIcon { busname: String },
    /// 左键点击图标。
    Activate { busname: String },
    /// 右键点击图标。
    OpenMenu { busname: String },
    /// 菜单里选了第 `index` 项；负数是取消。
    Pick { index: i32 },
    /// bar 的图标格变了，重新按该尺寸解码主题图标。
    SetIconSize(u32),
}

/// 合成器持有的控制句柄；服务线程已退出时发送会失败，静默忽略。
#[derive(Clone)]
pub struct TrayHandle {
    tx: mpsc::Sender<Command>,
}

impl TrayHandle {
    fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    /// 左键：调用 item 的 `Activate(0, 0)`。
    pub fn activate(&self, busname: &str) {
        self.send(Command::Activate {
            busname: busname.to_owned(),
        });
    }

    /// 右键：拉取该 item 的 dbusmenu。
    pub fn open_menu(&self, busname: &str) {
        self.send(Command::OpenMenu {
            busname: busname.to_owned(),
        });
    }

    /// 选中当前菜单的第 `index` 项；`-1` 表示取消。
    pub fn pick(&self, index: i32) {
        self.send(Command::Pick { index });
    }

    /// bar 的图标格尺寸变了，让服务线程按新尺寸重新解码主题图标。
    pub fn set_icon_size(&self, size: u32) {
        self.send(Command::SetIconSize(size));
    }
}

/// 启动托盘服务线程。没有会话总线时线程只告警并退出。
pub fn start(sender: calloop::channel::Sender<Snapshot>, icon_size: u32) -> Option<TrayHandle> {
    let (tx, rx) = mpsc::channel();
    let handle = TrayHandle { tx: tx.clone() };
    let names = Arc::new(Mutex::new(Vec::new()));
    let host = Host {
        tx: tx.clone(),
        names: names.clone(),
    };
    let spawned = thread::Builder::new()
        .name("r0wm-systray".into())
        .spawn(move || {
            let connection = Builder::session()
                // 坏客户端不回复时别把整个托盘线程卡死。
                .map(|builder| builder.method_timeout(std::time::Duration::from_secs(3)))
                .and_then(|builder| builder.name(SNI_HOST_NAME))
                .and_then(|builder| builder.serve_at(SNI_HOST_PATH, host))
                .and_then(|builder| builder.build());
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    tracing::warn!("托盘服务不可用（会话总线或 {SNI_HOST_NAME} 被占用）：{err:#}");
                    return;
                }
            };
            spawn_signal_threads(&connection, tx);
            let mut service = Service {
                connection,
                snapshot: sender,
                names,
                items: Vec::new(),
                menu: None,
                icon_size: icon_size.max(16),
            };
            service.run(rx);
        });
    match spawned {
        Ok(_) => Some(handle),
        Err(err) => {
            tracing::warn!("托盘线程启动失败：{err}");
            None
        }
    }
}

/// `org.kde.StatusNotifierWatcher` 接口。方法只把请求转给服务线程，
/// 真正的属性查询在那边做，避免占住 zbus 的 executor 线程。
struct Host {
    tx: mpsc::Sender<Command>,
    /// `RegisteredStatusNotifierItems` 属性直接从这份共享列表读。
    names: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Host {
    fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(|name| name.to_string());
        // 基线的三种形态：对象路径、唯一名（`:` 开头）或 KStatusNotifierItem
        // 的 well-known name。
        let (busname, busobj) = match service.chars().next() {
            Some('/') => (sender, service),
            Some(':') => (Some(service), SNI_ITEM_PATH.to_owned()),
            _ => (Some(service), SNI_ITEM_PATH.to_owned()),
        };
        let Some(busname) = busname else {
            return Err(zbus::fdo::Error::InvalidArgs("missing sender".into()));
        };
        if BusName::try_from(busname.as_str()).is_err() {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "invalid busname {busname}"
            )));
        }
        let _ = self.tx.send(Command::Register { busname, busobj });
        Ok(())
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.names
            .lock()
            .map(|names| names.clone())
            .unwrap_or_default()
    }
}

/// 服务线程里的一个 item。
struct Item {
    busname: String,
    busobj: String,
    appid: String,
    menu: Option<String>,
    icon: Option<Arc<Icon>>,
}

/// 正在显示的菜单。`shown` 是当前这一层的节点，进入子菜单时整体替换。
struct MenuState {
    busname: String,
    busobj: String,
    shown: Vec<MenuItem>,
}

/// dbusmenu 节点的原始形态：`(id, a{sv}, av)`。
type RawNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

struct Service {
    connection: Connection,
    snapshot: calloop::channel::Sender<Snapshot>,
    names: Arc<Mutex<Vec<String>>>,
    items: Vec<Item>,
    menu: Option<MenuState>,
    icon_size: u32,
}

impl Service {
    fn run(&mut self, rx: mpsc::Receiver<Command>) {
        while let Ok(command) = rx.recv() {
            match command {
                Command::Register { busname, busobj } => self.register(busname, busobj),
                Command::OwnerChanged { name, new_owner } => self.owner_changed(&name, &new_owner),
                Command::NewIcon { busname } => self.new_icon(&busname),
                Command::Activate { busname } => self.activate(&busname),
                Command::OpenMenu { busname } => self.open_menu(&busname),
                Command::Pick { index } => self.pick(index),
                Command::SetIconSize(size) => self.set_icon_size(size),
            }
        }
    }

    // ---- 注册 / 注销 ----

    fn register(&mut self, busname: String, busobj: String) {
        if self.items.iter().any(|item| item.busname == busname) {
            return;
        }
        // 先占位：属性查询可能要等客户端回复，`RegisteredStatusNotifierItems`
        // 应当立刻能看到它（基线 createitem 是同步的）。
        self.items.push(Item {
            busname: busname.clone(),
            busobj: busobj.clone(),
            appid: String::new(),
            menu: None,
            icon: None,
        });
        self.publish();
        let item = self.load_item(&busname, &busobj);
        if let Some(slot) = self.items.iter_mut().find(|item| item.busname == busname) {
            *slot = item;
        }
        self.publish();
    }

    fn owner_changed(&mut self, name: &str, new_owner: &str) {
        if !new_owner.is_empty() {
            return;
        }
        let Some(index) = self.items.iter().position(|item| item.busname == name) else {
            return;
        };
        self.items.remove(index);
        if self.menu.as_ref().is_some_and(|menu| menu.busname == name) {
            self.menu = None;
        }
        self.publish();
    }

    fn set_icon_size(&mut self, size: u32) {
        // 只在后续解码时生效；已解好的图标在绘制时统一缩进格子。
        self.icon_size = size.max(16);
    }

    // ---- 属性查询 ----

    fn load_item(&self, busname: &str, busobj: &str) -> Item {
        let appid = self
            .property(busname, busobj, "Id")
            .and_then(|value| value_string(&value))
            .unwrap_or_default();
        let menu = self
            .property(busname, busobj, "Menu")
            .and_then(|value| value_string(&value))
            .filter(|path| !path.is_empty());
        let icon = self
            .load_pixmap(busname, busobj)
            .or_else(|| self.load_themed(busname, busobj))
            .map(Arc::new);
        Item {
            busname: busname.to_owned(),
            busobj: busobj.to_owned(),
            appid,
            menu,
            icon,
        }
    }

    fn property(&self, busname: &str, busobj: &str, name: &str) -> Option<OwnedValue> {
        let reply = self
            .connection
            .call_method(
                Some(busname),
                busobj,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(SNI_IFACE, name),
            )
            .ok()?;
        reply.body().deserialize::<OwnedValue>().ok()
    }

    /// 取 `IconPixmap`（`a(iiay)`）里最合适的一张，转成 RGBA。
    fn load_pixmap(&self, busname: &str, busobj: &str) -> Option<Icon> {
        let value = self.property(busname, busobj, "IconPixmap")?;
        let pixmaps: Vec<(i32, i32, Vec<u8>)> = Vec::try_from(value).ok()?;
        if pixmaps.is_empty() {
            return None;
        }
        // 基线 `select_image`：优先第一个宽 >= 22 的，否则取最后一张。
        let index = pixmaps
            .iter()
            .position(|(width, _, _)| *width >= ICON_PREFERRED)
            .unwrap_or(pixmaps.len() - 1);
        let (width, height, data) = &pixmaps[index];
        let (width, height) = (*width, *height);
        if width <= 0 || height <= 0 || data.len() != (width as usize) * (height as usize) * 4 {
            return None;
        }
        // 规范给的是网络字节序的 ARGB32，这里换成未预乘的 RGBA。
        let mut rgba = Vec::with_capacity(data.len());
        for pixel in data.chunks_exact(4) {
            rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
        }
        Some(icon::shrink(
            &rgba,
            width as u32,
            height as u32,
            ICON_STORE_MAX,
        ))
    }

    /// `IconThemePath` 先查，再按 `IconName` 去 hicolor 里找（基线顺序）。
    fn load_themed(&self, busname: &str, busobj: &str) -> Option<Icon> {
        let name = self
            .property(busname, busobj, "IconName")
            .and_then(|value| value_string(&value))?;
        if name.is_empty() {
            return None;
        }
        let theme = self
            .property(busname, busobj, "IconThemePath")
            .and_then(|value| value_string(&value))
            .filter(|path| !path.is_empty());
        icon::load_named(
            &name,
            theme.as_deref().map(Path::new),
            self.icon_size.max(ICON_LOAD_SIZE),
        )
    }

    fn new_icon(&mut self, busname: &str) {
        let Some(index) = self.items.iter().position(|item| item.busname == busname) else {
            return;
        };
        let (busname, busobj) = (
            self.items[index].busname.clone(),
            self.items[index].busobj.clone(),
        );
        if let Some(icon) = self
            .load_pixmap(&busname, &busobj)
            .or_else(|| self.load_themed(&busname, &busobj))
        {
            self.items[index].icon = Some(Arc::new(icon));
            self.publish();
        }
    }

    // ---- 点击 / 菜单 ----

    fn activate(&self, busname: &str) {
        let Some(item) = self.items.iter().find(|item| item.busname == busname) else {
            return;
        };
        let _ = self.connection.call_method(
            Some(item.busname.as_str()),
            item.busobj.as_str(),
            Some(SNI_IFACE),
            "Activate",
            &(0i32, 0i32),
        );
    }

    fn open_menu(&mut self, busname: &str) {
        let Some(item) = self.items.iter().find(|item| item.busname == busname) else {
            return;
        };
        let Some(busobj) = item.menu.clone() else {
            return;
        };
        // 基线先 AboutToShow 再 GetLayout；两个回复都是短请求，顺序等待即可。
        let _ = self.connection.call_method(
            Some(busname),
            busobj.as_str(),
            Some(DBUSMENU_IFACE),
            "AboutToShow",
            &(0i32,),
        );
        let Ok(reply) = self.connection.call_method(
            Some(busname),
            busobj.as_str(),
            Some(DBUSMENU_IFACE),
            "GetLayout",
            &(0i32, -1i32, Vec::<String>::new()),
        ) else {
            return;
        };
        let Ok((_, (_, _, children))) = reply.body().deserialize::<(u32, RawNode)>() else {
            return;
        };
        let shown: Vec<MenuItem> = children
            .iter()
            .filter_map(raw_node)
            .take(MENU_ITEMS_MAX)
            .collect();
        if shown.is_empty() {
            return;
        }
        self.menu = Some(MenuState {
            busname: busname.to_owned(),
            busobj,
            shown,
        });
        self.publish();
    }

    fn pick(&mut self, index: i32) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        if index < 0 || index as usize >= menu.shown.len() {
            self.menu = None;
            self.publish();
            return;
        }
        let index = index as usize;
        let has_submenu = menu.shown[index].has_submenu;
        if has_submenu {
            let menu = self.menu.as_mut().expect("checked above");
            let children = std::mem::take(&mut menu.shown[index].children);
            if children.is_empty() {
                self.menu = None;
            } else {
                self.menu.as_mut().expect("checked above").shown = children;
            }
            self.publish();
            return;
        }

        let (id, busname, busobj) = {
            let menu = self.menu.as_ref().expect("checked above");
            (
                menu.shown[index].id,
                menu.busname.clone(),
                menu.busobj.clone(),
            )
        };
        self.send_clicked(&busname, &busobj, id);
        self.menu = None;
        self.publish();
    }

    fn send_clicked(&self, busname: &str, busobj: &str, itemid: i32) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as u32)
            .unwrap_or(0);
        let data = Value::from("");
        let _ = self.connection.call_method(
            Some(busname),
            busobj,
            Some(DBUSMENU_IFACE),
            "Event",
            &(itemid, "clicked", data, timestamp),
        );
    }

    // ---- 快照 ----

    fn publish(&self) {
        let items = self
            .items
            .iter()
            .map(|item| ItemView {
                busname: item.busname.clone(),
                appid: item.appid.clone(),
                icon: item.icon.clone(),
            })
            .collect();
        let menu = self
            .menu
            .as_ref()
            .and_then(|menu| MenuItem::view(&menu.shown));
        let _ = self.snapshot.send(Snapshot { items, menu });
        if let Ok(mut names) = self.names.lock() {
            *names = self.items.iter().map(|item| item.busname.clone()).collect();
        }
    }
}

/// 把 dbusmenu 的原始节点转成 [`MenuItem`]，过滤规则与基线 `read_dict` 一致：
/// 没有 label、不可见或不可用的条目直接丢弃。
fn parse_node(raw: RawNode) -> Option<MenuItem> {
    let (id, props, children) = raw;
    if !prop_bool(&props, "visible", true) || !prop_bool(&props, "enabled", true) {
        return None;
    }
    let label = prop_string(&props, "label").filter(|label| !label.is_empty())?;
    let toggle = prop_string(&props, "toggle-type").as_deref() == Some("checkmark");
    let toggle_state = if toggle {
        Some(prop_i32(&props, "toggle-state").unwrap_or(1))
    } else {
        None
    };
    let has_submenu = prop_string(&props, "children-display").as_deref() == Some("submenu");
    let children = if has_submenu {
        children
            .iter()
            .filter_map(raw_node)
            .take(MENU_ITEMS_MAX)
            .collect()
    } else {
        Vec::new()
    };
    Some(MenuItem {
        id,
        label: format_label(&label, toggle_state, has_submenu),
        has_submenu,
        children,
    })
}

fn prop_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props.get(key).and_then(value_string)
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str, default: bool) -> bool {
    props
        .get(key)
        .and_then(|value| value.downcast_ref::<bool>().ok())
        .unwrap_or(default)
}

fn prop_i32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<i32> {
    props
        .get(key)
        .and_then(|value| value.downcast_ref::<i32>().ok())
}

fn value_string(value: &OwnedValue) -> Option<String> {
    if let Ok(string) = value.downcast_ref::<String>() {
        return Some(string);
    }
    Value::try_from(value)
        .ok()?
        .downcast::<OwnedObjectPath>()
        .ok()
        .map(|path| path.to_string())
}

/// `av` 里的一个元素（结构体）→ 原始节点。
fn raw_node(value: &OwnedValue) -> Option<MenuItem> {
    let raw: RawNode = Value::try_from(value).ok()?.downcast::<RawNode>().ok()?;
    parse_node(raw)
}

// ---- 信号监听 ----

/// 起两个线程分别监听 `NameOwnerChanged` 与 item 的 `NewIcon`。
///
/// 两个匹配规则无法塞进同一个 `MessageIterator`，所以各用一份连接克隆。
/// 进程退出时线程随进程结束，不做显式回收。
fn spawn_signal_threads(connection: &Connection, tx: mpsc::Sender<Command>) {
    let conn = connection.clone();
    let names_tx = tx.clone();
    let _ = thread::Builder::new()
        .name("r0wm-systray-names".into())
        .spawn(move || {
            let Some(rule) = MatchRule::builder()
                .msg_type(Type::Signal)
                .sender("org.freedesktop.DBus")
                .ok()
                .and_then(|builder| builder.interface("org.freedesktop.DBus").ok())
                .and_then(|builder| builder.member("NameOwnerChanged").ok())
                .map(|builder| builder.build())
            else {
                return;
            };
            let Ok(iterator) = MessageIterator::for_match_rule(rule, &conn, Some(64)) else {
                return;
            };
            for message in iterator {
                let Ok(message) = message else {
                    continue;
                };
                let Ok((name, _old, new_owner)) =
                    message.body().deserialize::<(String, String, String)>()
                else {
                    continue;
                };
                let _ = names_tx.send(Command::OwnerChanged { name, new_owner });
            }
        });

    let conn = connection.clone();
    let icon_tx = tx;
    let _ = thread::Builder::new()
        .name("r0wm-systray-icons".into())
        .spawn(move || {
            let Some(rule) = MatchRule::builder()
                .msg_type(Type::Signal)
                .interface(SNI_IFACE)
                .ok()
                .and_then(|builder| builder.member("NewIcon").ok())
                .map(|builder| builder.build())
            else {
                return;
            };
            let Ok(iterator) = MessageIterator::for_match_rule(rule, &conn, Some(64)) else {
                return;
            };
            for message in iterator {
                let Ok(message) = message else {
                    continue;
                };
                let sender = message.header().sender().map(|name| name.to_string());
                let Some(busname) = sender else {
                    continue;
                };
                let _ = icon_tx.send(Command::NewIcon { busname });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Structure;

    fn owned(value: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(value).expect("value must own")
    }

    fn text(value: &str) -> OwnedValue {
        owned(Value::from(value.to_owned()))
    }

    fn props(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.try_clone().unwrap()))
            .collect()
    }

    fn raw(id: i32, pairs: &[(&str, OwnedValue)], children: Vec<RawNode>) -> RawNode {
        (
            id,
            props(pairs),
            children.into_iter().map(owned_node).collect(),
        )
    }

    fn owned_node(node: RawNode) -> OwnedValue {
        owned(Value::from(Structure::from(node)))
    }

    #[test]
    fn leaf_node_gets_alignment_prefix() {
        let node = raw(7, &[("label", text("Open"))], Vec::new());
        let item = parse_node(node).expect("visible leaf");
        assert_eq!(item.id, 7);
        assert_eq!(item.label, "  Open");
        assert!(!item.has_submenu);
        assert!(item.children.is_empty());
    }

    #[test]
    fn hidden_or_disabled_or_unlabelled_nodes_are_dropped() {
        for pairs in [
            vec![("label", text("x")), ("visible", owned(Value::from(false)))],
            vec![("label", text("x")), ("enabled", owned(Value::from(false)))],
            vec![("visible", owned(Value::from(true)))],
            vec![("label", text(""))],
        ] {
            assert!(
                parse_node(raw(1, &pairs, Vec::new())).is_none(),
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn checkmark_and_submenu_are_formatted() {
        let child = raw(2, &[("label", text("Inner"))], Vec::new());
        let node = raw(
            1,
            &[
                ("label", text("Show")),
                ("toggle-type", text("checkmark")),
                ("toggle-state", owned(Value::from(1))),
                ("children-display", text("submenu")),
            ],
            vec![child],
        );
        let item = parse_node(node).expect("submenu");
        assert_eq!(item.label, "✓ Show →");
        assert!(item.has_submenu);
        assert_eq!(item.children.len(), 1);
        assert_eq!(item.children[0].label, "  Inner");
    }

    #[test]
    fn non_checkmark_toggle_keeps_alignment_prefix() {
        let node = raw(
            1,
            &[
                ("label", text("Radio")),
                ("toggle-type", text("radio")),
                ("toggle-state", owned(Value::from(1))),
            ],
            Vec::new(),
        );
        assert_eq!(parse_node(node).unwrap().label, "  Radio");
    }

    #[test]
    fn nested_submenus_are_parsed_recursively() {
        let leaf = raw(3, &[("label", text("Leaf"))], Vec::new());
        let inner = raw(
            2,
            &[
                ("label", text("More")),
                ("children-display", text("submenu")),
            ],
            vec![leaf],
        );
        let item = parse_node(raw(
            1,
            &[
                ("label", text("Top")),
                ("children-display", text("submenu")),
            ],
            vec![inner],
        ))
        .unwrap();
        assert_eq!(item.children[0].label, "  More →");
        assert_eq!(item.children[0].children[0].label, "  Leaf");
    }
}
