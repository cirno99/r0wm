//! `org.freedesktop.Notifications` 的 D-Bus 服务。
//!
//! 线程模型：D-Bus 连接与状态机都跑在一个独立线程里，合成器通过
//! [`NotifyHandle`] 发命令，服务线程把 [`Snapshot`] 推回 calloop channel。
//! 这样渲染线程里没有 async 运行时，通知的 id 也在 `Notify` 方法里同步返回。
//!
//! 总线不可用（或名字已被 mako/dunst 之类占用）时只告警并退出线程，合成器
//! 照常运行，只是没有通知（基线同样把 D-Bus 失败当作非致命）。

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use zbus::{
    blocking::{Connection, connection::Builder},
    zvariant::{Array, OwnedValue, Structure},
};

use super::{ACT_MAX, Action, ICON_SIZE, Icon, Incoming, Snapshot, Store, Urgency};

/// 总线上的服务名。
pub const NAME: &str = "org.freedesktop.Notifications";
/// 对象路径。
pub const PATH: &str = "/org/freedesktop/Notifications";
/// 接口名。
pub const IFACE: &str = "org.freedesktop.Notifications";

/// 合成器发给服务线程的命令。
#[derive(Debug)]
enum Command {
    /// 内部唤醒：把队列里的信号发出去。
    Wake,
    Dismiss,
    DismissAll,
    Invoke {
        token: Option<String>,
    },
    Action {
        index: usize,
        token: Option<String>,
    },
    Hold(bool),
    HistoryRemove(u32),
    SetTimeout(u32),
}

/// 服务线程要发出的 D-Bus 信号。
#[derive(Debug)]
enum Signal {
    Closed { id: u32, reason: u32 },
    ActionInvoked { id: u32, key: String },
    ActivationToken { id: u32, token: String },
}

/// 合成器持有的控制句柄；发送失败（服务线程已退出）时静默忽略。
#[derive(Clone)]
pub struct NotifyHandle {
    tx: mpsc::Sender<Command>,
}

impl NotifyHandle {
    fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    /// 关掉当前这条，队列里的下一条接上。
    pub fn dismiss(&self) {
        self.send(Command::Dismiss);
    }

    /// 当前与排队的全部关掉。
    pub fn dismiss_all(&self) {
        self.send(Command::DismissAll);
    }

    /// 触发当前通知的 `default` 动作；`token` 是给客户端的激活令牌。
    pub fn invoke(&self, token: Option<String>) {
        self.send(Command::Invoke { token });
    }

    /// 触发第 `index` 个动作。
    pub fn action(&self, index: usize, token: Option<String>) {
        self.send(Command::Action { index, token });
    }

    /// 挑动作时暂停计时。
    pub fn hold(&self, on: bool) {
        self.send(Command::Hold(on));
    }

    /// 从历史里删掉一条。
    pub fn history_remove(&self, id: u32) {
        self.send(Command::HistoryRemove(id));
    }

    /// 热重载配置后更新默认超时（秒）。
    pub fn set_timeout(&self, seconds: u32) {
        self.send(Command::SetTimeout(seconds));
    }
}

/// 启动服务线程。模拟环境没有会话总线时返回 `None`。
pub fn start(
    sender: calloop::channel::Sender<Snapshot>,
    timeout_secs: u32,
) -> Option<NotifyHandle> {
    let (tx, rx) = mpsc::channel();
    let handle = NotifyHandle { tx: tx.clone() };
    let clock = Instant::now();
    let spawned = thread::Builder::new()
        .name("r0wm-notify".into())
        .spawn(move || {
            let shared = Arc::new(Mutex::new(Shared {
                store: Store::new(timeout_secs),
                signals: Vec::new(),
                pending_token: None,
                snapshot: sender,
                wake: tx,
                clock,
            }));
            let notifier = Notifier {
                shared: shared.clone(),
            };
            // 名字被别的守护占用时不要硬抢：直接停掉自己（基线同样只告警）。
            let connection = Builder::session()
                .and_then(|builder| builder.name(NAME))
                .and_then(|builder| builder.serve_at(PATH, notifier))
                .and_then(|builder| builder.build());
            match connection {
                Ok(connection) => run(rx, shared, connection),
                Err(err) => tracing::warn!(
                    "通知服务不可用（会话总线或 {NAME} 被占用）：{err:#}；bar 不显示通知"
                ),
            }
        });
    match spawned {
        Ok(_) => Some(handle),
        Err(err) => {
            tracing::warn!("通知线程启动失败：{err}");
            None
        }
    }
}

/// D-Bus 接口实现。方法体都是同步的短临界区，async 只是 zbus 的形式要求。
struct Notifier {
    shared: Arc<Mutex<Shared>>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifier {
    /// 支持的能力；与基线一致（body / body-markup / actions / icon-static）。
    async fn get_capabilities(&self) -> Vec<String> {
        ["body", "body-markup", "actions", "icon-static"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "r0wm".into(),
            "r0wm".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let incoming = parse_incoming(
            &app_name,
            replaces_id,
            &app_icon,
            &summary,
            &body,
            &actions,
            &hints,
            expire_timeout,
        );
        let mut shared = self.shared.lock().unwrap();
        let now = shared.now();
        let id = shared.store.notify(now, incoming);
        shared.finish_and_wake();
        id
    }

    async fn close_notification(&self, id: u32) {
        let mut shared = self.shared.lock().unwrap();
        let now = shared.now();
        if shared.store.close(now, id) {
            shared.finish_and_wake();
        }
    }
}

/// 服务线程共享的状态。
struct Shared {
    store: Store,
    signals: Vec<Signal>,
    /// 下一次 `ActionInvoked` 之前要先发的激活令牌。
    pending_token: Option<String>,
    snapshot: calloop::channel::Sender<Snapshot>,
    wake: mpsc::Sender<Command>,
    clock: Instant,
}

impl Shared {
    fn now(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    /// 把状态机的副作用翻译成信号，再把快照推给合成器。
    fn finish(&mut self) {
        for event in self.store.drain_events() {
            match event {
                super::Event::Closed { id, reason } => self.signals.push(Signal::Closed {
                    id,
                    reason: reason.as_u32(),
                }),
                super::Event::ActionInvoked { id, key } => {
                    if let Some(token) = self.pending_token.take() {
                        self.signals.push(Signal::ActivationToken { id, token });
                    }
                    self.signals.push(Signal::ActionInvoked { id, key });
                }
            }
        }
        self.pending_token = None;
        let snapshot = self.store.snapshot(self.now());
        let _ = self.snapshot.send(snapshot);
    }

    /// 接口方法（在 zbus 线程上）改完状态后，要唤醒服务线程发信号。
    fn finish_and_wake(&mut self) {
        self.finish();
        let _ = self.wake.send(Command::Wake);
    }
}

fn run(rx: mpsc::Receiver<Command>, shared: Arc<Mutex<Shared>>, connection: Connection) {
    loop {
        let wait = {
            let shared = shared.lock().unwrap();
            next_wait(&shared.store, shared.now())
        };
        let command = match rx.recv_timeout(wait) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => Command::Wake,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let signals = {
            let mut shared = shared.lock().unwrap();
            let now = shared.now();
            let is_wake = matches!(command, Command::Wake);
            if !is_wake {
                apply(&mut shared, command, now);
            }
            // 超时也要走一遍 tick：到点就关，没到点则只推进进度条。
            shared.store.tick(now);
            shared.finish();
            std::mem::take(&mut shared.signals)
        };
        emit(&connection, signals);
    }
}

/// 下一次需要醒来处理的时间：到期时刻与进度条的下一格取较小者。
fn next_wait(store: &Store, now: u64) -> Duration {
    let Some(deadline) = store.deadline() else {
        // 没有计时任务时睡久一点；命令到达会立即唤醒。
        return Duration::from_secs(3600);
    };
    let remaining = deadline.saturating_sub(now);
    let total = store.current().map(|cur| cur.timeout_ms).unwrap_or(0) as u64;
    // 基线以字体行高（约 15）为进度条格数，最短 16ms。
    let step = (total / 15).max(16);
    Duration::from_millis(remaining.min(step).max(1))
}

fn apply(shared: &mut Shared, command: Command, now: u64) {
    match command {
        Command::Wake => {}
        Command::Dismiss => shared.store.dismiss(now),
        Command::DismissAll => shared.store.dismiss_all(now),
        Command::Invoke { token } => {
            shared.pending_token = token;
            shared.store.invoke(now);
        }
        Command::Action { index, token } => {
            shared.pending_token = token;
            shared.store.action(now, index);
        }
        Command::Hold(on) => shared.store.hold(now, on),
        Command::HistoryRemove(id) => {
            shared.store.history_remove(id);
        }
        Command::SetTimeout(seconds) => shared.store.set_timeout_secs(seconds),
    }
}

fn emit(connection: &Connection, signals: Vec<Signal>) {
    for signal in signals {
        let result = match signal {
            Signal::Closed { id, reason } => connection.emit_signal(
                None::<&str>,
                PATH,
                IFACE,
                "NotificationClosed",
                &(id, reason),
            ),
            Signal::ActionInvoked { id, key } => {
                connection.emit_signal(None::<&str>, PATH, IFACE, "ActionInvoked", &(id, key))
            }
            Signal::ActivationToken { id, token } => {
                connection.emit_signal(None::<&str>, PATH, IFACE, "ActivationToken", &(id, token))
            }
        };
        if let Err(err) = result {
            tracing::warn!("发送通知信号失败：{err}");
        }
    }
}

/// 把 `Notify` 的原始参数（含 hints）摊平成状态机认识的字段。
#[allow(clippy::too_many_arguments)]
fn parse_incoming(
    app_name: &str,
    replaces_id: u32,
    app_icon: &str,
    summary: &str,
    body: &str,
    actions: &[String],
    hints: &HashMap<String, OwnedValue>,
    expire_timeout: i32,
) -> Incoming {
    let mut incoming = Incoming {
        replaces_id,
        app: app_name.to_string(),
        summary: summary.to_string(),
        body: body.to_string(),
        urgency: hint_u8(hints, "urgency")
            .map(Urgency::from_hint)
            .unwrap_or_default(),
        transient: hint_bool(hints, "transient"),
        resident: hint_bool(hints, "resident"),
        timeout_ms: expire_timeout,
        value: hint_value(hints),
        desktop: hint_string(hints, "desktop-entry").unwrap_or_default(),
        tag: hint_string(hints, "x-dunst-stack-tag")
            .or_else(|| hint_string(hints, "x-canonical-private-synchronous"))
            .unwrap_or_default(),
        icon: resolve_icon(hints, app_icon),
        ..Default::default()
    };

    // actions 是 (key, label, key, label, ...) 的扁平列表。
    for pair in actions.chunks_exact(2) {
        let (key, label) = (&pair[0], &pair[1]);
        if key == "default" {
            incoming.has_default = true;
        } else if incoming.actions.len() < ACT_MAX && key.len() < 64 {
            incoming.actions.push(Action {
                key: key.clone(),
                label: if label.is_empty() {
                    key.clone()
                } else {
                    label.clone()
                },
            });
        }
    }
    incoming
}

fn hint_string(hints: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    hints
        .get(name)?
        .try_clone()
        .ok()
        .and_then(|value| String::try_from(value).ok())
}

fn hint_bool(hints: &HashMap<String, OwnedValue>, name: &str) -> bool {
    hints
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn hint_u8(hints: &HashMap<String, OwnedValue>, name: &str) -> Option<u8> {
    u8::try_from(hints.get(name)?).ok()
}

/// `value` 客户端可能发 i32 也可能发 u32；超范围夹到 0..=100。
fn hint_value(hints: &HashMap<String, OwnedValue>) -> Option<u8> {
    let value = hints.get("value")?;
    let number = i32::try_from(value)
        .ok()
        .or_else(|| u32::try_from(value).ok().map(|v| v.min(100) as i32))?;
    Some(number.clamp(0, 100) as u8)
}

/// 按规范的优先级找图标（基线 `notify_icon` 的顺序）：
/// `image-data` / `image_data` → `image-path` / `image_path` → `app_icon`
/// → 老式的 `icon_data`。
///
/// 后两个来源可以是文件路径、`file://` URI 或图标名；名字会去 hicolor 主题里
/// 查（[`ui::icon`](crate::ui::icon)）。
fn resolve_icon(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> Option<Arc<Icon>> {
    image_hint(hints, &["image-data", "image_data"])
        .or_else(|| {
            ["image-path", "image_path"]
                .into_iter()
                .find_map(|name| hint_string(hints, name).and_then(|path| icon_named(&path)))
        })
        .or_else(|| icon_named(app_icon))
        .or_else(|| image_hint(hints, &["icon_data"]))
}

fn image_hint(hints: &HashMap<String, OwnedValue>, names: &[&str]) -> Option<Arc<Icon>> {
    names
        .iter()
        .find_map(|name| hints.get(*name).and_then(decode_image_hint))
}

/// `(iiibiiay)`：宽、高、行跨距、alpha、位深、通道数、数据。
fn decode_image_hint(value: &OwnedValue) -> Option<Arc<Icon>> {
    let structure = Structure::try_from(value.try_clone().ok()?).ok()?;
    let fields = structure.fields();
    let width: i32 = fields.first()?.try_into().ok()?;
    let height: i32 = fields.get(1)?.try_into().ok()?;
    let rowstride: i32 = fields.get(2)?.try_into().ok()?;
    let alpha: bool = fields.get(3)?.try_into().ok()?;
    let bits: i32 = fields.get(4)?.try_into().ok()?;
    let channels: i32 = fields.get(5)?.try_into().ok()?;
    let array = Array::try_from(fields.get(6)?.try_clone().ok()?).ok()?;
    let data: Vec<u8> = Vec::try_from(array).ok()?;

    let expected_channels = if alpha { 4 } else { 3 };
    if width <= 0
        || height <= 0
        || width > super::ICON_SIDE_MAX
        || height > super::ICON_SIDE_MAX
        || bits != 8
        || channels != expected_channels
        || rowstride < width * channels
        || (data.len() as i64)
            < (rowstride as i64) * (height as i64 - 1) + (width as i64) * (channels as i64)
    {
        return None;
    }
    Some(Arc::new(scale_icon(
        &data,
        width,
        height,
        rowstride as usize,
        alpha,
    )))
}
/// 把 `image-data` 缩到 [`ICON_SIZE`](super::ICON_SIZE) 以内（只缩不放，
/// 双线性重采样，和基线 `createiconfromdata` 的语义一致）。
fn scale_icon(data: &[u8], width: i32, height: i32, rowstride: usize, alpha: bool) -> Icon {
    let (width, height) = (width as u32, height as u32);
    let channels = if alpha { 4usize } else { 3usize };
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let row = &data[y as usize * rowstride..];
        for x in 0..width {
            let i = x as usize * channels;
            rgba.extend_from_slice(&row[i..i + 3]);
            rgba.push(if alpha { row[i + 3] } else { 0xff });
        }
    }
    crate::ui::icon::shrink(&rgba, width, height, ICON_SIZE as u32)
}

/// 文件 / `file://` URI / 图标名 → 位图。主题查找与缓存都在
/// [`ui::icon`](crate::ui::icon) 里，这里只负责换成通知用的共享指针。
fn icon_named(name: &str) -> Option<Arc<Icon>> {
    crate::ui::icon::load_named(name, None, ICON_SIZE as u32).map(Arc::new)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{ICON_SIZE, QUEUE_MAX, TEXT_MAX};

    fn hints(pairs: Vec<(&str, OwnedValue)>) -> HashMap<String, OwnedValue> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn parses_actions_and_default() {
        let incoming = parse_incoming(
            "app",
            7,
            "",
            "summary",
            "body",
            &[
                "default".into(),
                "Open".into(),
                "reply".into(),
                "Reply".into(),
            ],
            &HashMap::new(),
            -1,
        );
        assert_eq!(incoming.replaces_id, 7);
        assert_eq!(incoming.timeout_ms, -1);
        assert!(incoming.has_default);
        assert_eq!(incoming.actions.len(), 1);
        assert_eq!(incoming.actions[0].key, "reply");
        assert_eq!(incoming.actions[0].label, "Reply");
        assert!(incoming.body.ends_with("body"));
    }

    #[test]
    fn parses_hints() {
        let map = hints(vec![
            ("urgency", OwnedValue::from(2u8)),
            ("transient", OwnedValue::from(true)),
            (
                "desktop-entry",
                OwnedValue::from(zbus::zvariant::Str::from("foot")),
            ),
            (
                "x-dunst-stack-tag",
                OwnedValue::from(zbus::zvariant::Str::from("vol")),
            ),
            ("value", OwnedValue::from(140u32)),
        ]);
        let incoming = parse_incoming("app", 0, "", "s", "", &[], &map, 1000);
        assert_eq!(incoming.urgency, Urgency::Critical);
        assert!(incoming.transient);
        assert_eq!(incoming.desktop, "foot");
        assert_eq!(incoming.tag, "vol");
        assert_eq!(incoming.value, Some(100));
    }

    #[test]
    fn value_hint_accepts_signed_and_clamps() {
        let map = hints(vec![("value", OwnedValue::from(-5i32))]);
        let incoming = parse_incoming("app", 0, "", "s", "", &[], &map, -1);
        assert_eq!(incoming.value, Some(0));
    }

    #[test]
    fn image_hint_is_decoded() {
        // 2x2 RGBA：红、绿、蓝、白。
        let data: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let mut builder = zbus::zvariant::StructureBuilder::new();
        builder = builder
            .add_field(2i32)
            .add_field(2i32)
            .add_field(8i32)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(data);
        let structure = builder.build();
        let value = OwnedValue::try_from(zbus::zvariant::Value::from(structure.unwrap())).unwrap();
        let icon = decode_image_hint(&value).expect("icon decodes");
        assert_eq!(icon.width, 2);
        assert_eq!(icon.height, 2);
        assert_eq!(icon.rgba.len(), 16);
        assert_eq!(&icon.rgba[0..3], &[255, 0, 0]);
        assert_eq!(&icon.rgba[4..7], &[0, 255, 0]);
    }

    #[test]
    fn oversized_icon_is_scaled_down() {
        // 200x100 RGB，全部同一个颜色；行跨距允许大于 width * 3。
        let width = 200usize;
        let height = 100usize;
        let rowstride = width * 3 + 16;
        let data = vec![7u8; rowstride * height];
        let mut builder = zbus::zvariant::StructureBuilder::new();
        builder = builder
            .add_field(width as i32)
            .add_field(height as i32)
            .add_field(rowstride as i32)
            .add_field(false)
            .add_field(8i32)
            .add_field(3i32)
            .add_field(data);
        let value =
            OwnedValue::try_from(zbus::zvariant::Value::from(builder.build().unwrap())).unwrap();
        let icon = decode_image_hint(&value).expect("icon decodes");
        assert_eq!(icon.width, ICON_SIZE);
        assert_eq!(icon.height, ICON_SIZE / 2);
        assert_eq!(&icon.rgba[0..4], &[7, 7, 7, 255]);
    }

    #[test]
    fn malformed_image_hint_is_rejected() {
        let mut builder = zbus::zvariant::StructureBuilder::new();
        builder = builder
            .add_field(10i32)
            .add_field(10i32)
            .add_field(1000i32)
            .add_field(false)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(vec![0u8; 4]);
        let value =
            OwnedValue::try_from(zbus::zvariant::Value::from(builder.build().unwrap())).unwrap();
        assert!(decode_image_hint(&value).is_none());
    }

    /// 造一块纯色 `(iiibiiay)`。
    fn solid_image(width: i32, height: i32, pixel: [u8; 4]) -> OwnedValue {
        let data: Vec<u8> = pixel
            .iter()
            .copied()
            .cycle()
            .take((width * height) as usize * 4)
            .collect();
        let structure = zbus::zvariant::StructureBuilder::new()
            .add_field(width)
            .add_field(height)
            .add_field(width * 4)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(data)
            .build()
            .unwrap();
        OwnedValue::try_from(zbus::zvariant::Value::from(structure)).unwrap()
    }

    /// 写一个单色 PNG，返回路径字符串。
    fn temp_png(tag: &str, color: [u8; 4]) -> String {
        let dir =
            std::env::temp_dir().join(format!("r0wm-notify-icon-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("icon.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba(color))
            .save(&path)
            .unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn image_path_accepts_file_uri_and_plain_path() {
        let path = temp_png("uri", [1, 2, 3, 255]);
        for value in [format!("file://{path}"), path.clone()] {
            let map = hints(vec![(
                "image-path",
                OwnedValue::from(zbus::zvariant::Str::from(value)),
            )]);
            let icon = resolve_icon(&map, "").expect("icon loads");
            assert_eq!((icon.width, icon.height), (ICON_SIZE, ICON_SIZE));
            assert_eq!(&icon.rgba[0..4], &[1, 2, 3, 255]);
        }
    }

    #[test]
    fn image_data_beats_image_path() {
        let path = temp_png("priority", [255, 0, 0, 255]);
        let map = hints(vec![
            ("image-data", solid_image(2, 2, [0, 255, 0, 255])),
            (
                "image-path",
                OwnedValue::from(zbus::zvariant::Str::from(path)),
            ),
        ]);
        let icon = resolve_icon(&map, "").expect("icon decodes");
        assert_eq!((icon.width, icon.height), (2, 2));
        assert_eq!(&icon.rgba[0..3], &[0, 255, 0]);
    }

    #[test]
    fn app_icon_is_the_fallback_before_icon_data() {
        let path = temp_png("appicon", [4, 5, 6, 255]);
        // app_icon 可以是路径（基线把它交给同一个 createiconfromname）。
        let map = hints(vec![("icon_data", solid_image(2, 2, [255, 0, 0, 255]))]);
        let icon = resolve_icon(&map, &path).expect("app icon loads");
        assert_eq!(&icon.rgba[0..4], &[4, 5, 6, 255]);

        // 没有 app_icon 时才轮到 icon_data。
        let icon = resolve_icon(&map, "").expect("icon_data decodes");
        assert_eq!((icon.width, icon.height), (2, 2));
    }

    #[test]
    fn unknown_icon_names_are_none() {
        let map = hints(vec![]);
        assert!(resolve_icon(&map, "r0wm-does-not-exist").is_none());
        assert!(icon_named("").is_none());
    }

    #[test]
    fn text_limit_matches_state_machine() {
        let long = "y".repeat(TEXT_MAX * 2);
        let incoming = parse_incoming("app", 0, "", "", &long, &[], &HashMap::new(), 0);
        assert!(
            crate::notify::compose_text(&incoming.app, &incoming.summary, &incoming.body).len()
                < TEXT_MAX
        );
    }

    #[test]
    fn next_wait_tracks_deadline_and_progress_steps() {
        let mut store = Store::new(10);
        store.notify(
            0,
            Incoming {
                app: "a".into(),
                summary: "s".into(),
                ..Default::default()
            },
        );
        // 10s 超时，15 格 -> 每格 666ms；离到期还有 10s，取较小者。
        let wait = next_wait(&store, 0);
        assert_eq!(wait, Duration::from_millis(666));
        // 临近到期时按剩余时间。
        let wait = next_wait(&store, 9900);
        assert_eq!(wait, Duration::from_millis(100));
    }

    #[test]
    fn queue_cap_is_exposed_to_the_service() {
        assert_eq!(QUEUE_MAX, 32);
    }
}
