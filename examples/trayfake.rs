//! 嵌套托盘冒烟用的假 StatusNotifierItem（不是 r0wm 的一部分）。
//!
//! 在同一个会话总线上注册一个带图标的 item，并暴露一个两项的 dbusmenu，
//! 用来验证 r0wm 的 watcher、图标解码、左键 `Activate` 与右键菜单。
//!
//! ```sh
//! dbus-run-session -- sh -c '
//!   ./target/debug/examples/trayfake &
//!   ./target/debug/r0wm --config ...
//!   # 左键点图标 → 日志打印 activate；右键 → 弹出 Open/Quit
//! '
//! ```

use std::{collections::HashMap, thread, time::Duration};

use zbus::{
    blocking::connection::Builder,
    zvariant::{ObjectPath, OwnedValue, Structure, Value},
};

const NAME: &str = "org.example.TrayFake";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/Menu";

struct Item;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn id(&self) -> String {
        NAME.into()
    }

    /// 24×24 的橙色方块；规范是网络字节序的 ARGB32。
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let mut pixels = Vec::with_capacity(24 * 24 * 4);
        for _ in 0..24 * 24 {
            pixels.extend_from_slice(&[0xff, 0xff, 0x8c, 0x00]);
        }
        vec![(24, 24, pixels)]
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::try_from(MENU_PATH).expect("static path")
    }

    fn activate(&self, x: i32, y: i32) {
        println!("activate {x} {y}");
    }
}

struct Menu;

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn about_to_show(&self, id: i32) -> bool {
        id == 0
    }

    #[allow(clippy::type_complexity)]
    fn get_layout(
        &self,
        parent_id: i32,
        _recursion: i32,
        _names: Vec<String>,
    ) -> (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)) {
        let _ = parent_id;
        let children = vec![node(1, "Open"), node(2, "Quit")];
        (0, (0, HashMap::new(), children))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        println!("event {id} {event_id}");
    }
}

fn node(id: i32, label: &str) -> OwnedValue {
    let mut props: HashMap<String, OwnedValue> = HashMap::new();
    props.insert("label".into(), owned(Value::from(label.to_owned())));
    props.insert("visible".into(), owned(Value::from(true)));
    props.insert("enabled".into(), owned(Value::from(true)));
    let children: Vec<OwnedValue> = Vec::new();
    owned(Value::from(Structure::from((id, props, children))))
}

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).expect("value must own")
}

fn main() -> zbus::Result<()> {
    let connection = Builder::session()?
        .name(NAME)?
        .serve_at(ITEM_PATH, Item)?
        .serve_at(MENU_PATH, Menu)?
        .build()?;
    connection.call_method(
        Some("org.kde.StatusNotifierWatcher"),
        "/StatusNotifierWatcher",
        Some("org.kde.StatusNotifierWatcher"),
        "RegisterStatusNotifierItem",
        &(NAME,),
    )?;
    println!("trayfake ready as {NAME}");
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
