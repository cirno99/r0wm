//! IPC：Unix socket + 行分隔 JSON（`docs/adr/0007`）。
//!
//! 三类消息：命令（走动作表）、查询（返回 JSON）、订阅（事件流）。
//! 事件格式从第一天带版本号，因为它是公开 API。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod server;

pub use server::IpcServer;

/// 协议版本。破坏性变更必须递增。
pub const PROTOCOL_VERSION: u32 = 1;

/// `$XDG_RUNTIME_DIR/r0wm.<WAYLAND_DISPLAY>.sock`；环境变量缺失时返回 `None`。
pub fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var("WAYLAND_DISPLAY").ok()?;
    Some(socket_path_in(&PathBuf::from(runtime), &display))
}

pub fn socket_path_in(runtime_dir: &std::path::Path, display: &str) -> PathBuf {
    runtime_dir.join(format!("r0wm.{display}.sock"))
}

/// 客户端发来的一条请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// 动作名 + 参数，与键绑定同一套解析。
    Command {
        action: String,
        #[serde(default)]
        arg: Option<String>,
    },
    Query {
        what: Query,
    },
    Subscribe {
        #[serde(default)]
        topics: Vec<Topic>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    Clients,
    Tags,
    Outputs,
    Focused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Focus,
    Tag,
    Title,
    Urgent,
    Layout,
    Output,
}

/// 服务端推给订阅者的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Hello {
        version: u32,
    },
    Focus {
        window: Option<u64>,
    },
    Tag {
        output: String,
        tags: u32,
    },
    Title {
        window: u64,
        title: String,
    },
    Layout {
        output: String,
        layout: String,
    },
    /// 输出模式/缩放/工作区变化；bar 用它重算自己的尺寸。
    Output {
        output: String,
        width: i32,
        height: i32,
        refresh: i32,
        scale: f64,
        work_x: i32,
        work_y: i32,
        work_width: i32,
        work_height: i32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_shape() {
        assert_eq!(
            socket_path_in(std::path::Path::new("/run/user/1000"), "wayland-1"),
            PathBuf::from("/run/user/1000/r0wm.wayland-1.sock")
        );
    }

    #[test]
    fn request_roundtrips() {
        let request = Request::Command {
            action: "view".into(),
            arg: Some("3".into()),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"type":"command","action":"view","arg":"3"}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn event_carries_version() {
        let json = serde_json::to_string(&Event::Hello {
            version: PROTOCOL_VERSION,
        })
        .unwrap();
        assert_eq!(json, r#"{"event":"hello","version":1}"#);
    }

    #[test]
    fn output_event_shape() {
        let json = serde_json::to_string(&Event::Output {
            output: "winit".into(),
            width: 1280,
            height: 800,
            refresh: 60_000,
            scale: 1.0,
            work_x: 0,
            work_y: 0,
            work_width: 1280,
            work_height: 800,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"event":"output","output":"winit","width":1280,"height":800,"refresh":60000,"scale":1.0,"work_x":0,"work_y":0,"work_width":1280,"work_height":800}"#
        );
    }
}
