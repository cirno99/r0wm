//! IPC 服务端：`UnixListener` + 行分隔 JSON（`docs/adr/0007`）。
//!
//! 每个连接注册成 calloop 的读事件源；回复立即写回同一套接字。
//! 内核缓冲满（客户端读得太慢）就直接断开，绝不阻塞合成器主循环。
//!
//! 请求处理本身在 [`State`] 上（`impl State` 在本文件下半部分）：
//! 命令走同一个动作表，查询读 `Shell`/`Space`，订阅把事件编码成 JSON 行。

use std::{
    io::{ErrorKind, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::json;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken, generic::Generic,
};

use crate::{
    action::Action,
    compositor::State,
    ipc::{Event, PROTOCOL_VERSION, Query, Request, Topic, socket_path_in},
    shell::Layer,
};

/// 单行请求的上限；超过就当作协议错误断开。
const MAX_REQUEST_LINE: usize = 8 * 1024;
/// 读取缓冲上限，防止客户端只写不换行把内存吃光。
const MAX_READ_BUFFER: usize = 64 * 1024;

/// 一个 IPC 连接。
struct Client {
    id: u64,
    token: RegistrationToken,
    /// 写回复用的句柄；与事件循环里登记的读端是同一套接字的克隆。
    stream: UnixStream,
    /// 尚未消费的请求字节。
    read: Vec<u8>,
    /// `None` 表示没有订阅；`Some([])` 表示订阅全部主题。
    topics: Option<Vec<Topic>>,
}

impl Client {
    fn subscribed(&self, topic: Topic) -> bool {
        self.topics
            .as_ref()
            .is_some_and(|topics| topics.is_empty() || topics.contains(&topic))
    }
}

/// IPC 服务端。
pub struct IpcServer {
    path: PathBuf,
    handle: LoopHandle<'static, State>,
    listener_token: RegistrationToken,
    clients: Vec<Client>,
    next_id: u64,
    /// 当前正在派发的连接；在这个回调里不能 `remove` 自己，只能返回 `PostAction::Remove`。
    dispatching: Option<u64>,
    /// 当前连接已被判定应断开。
    drop_current: bool,
    /// 上次广播过的状态，用于去重。
    last_focus: Option<Option<u64>>,
    last_tags: Option<u32>,
    last_layout: Option<String>,
    last_output: Option<Event>,
}

impl IpcServer {
    /// 绑定 `$XDG_RUNTIME_DIR/r0wm.<display>.sock`（0600）并挂进事件循环。
    pub fn start(event_loop: &mut EventLoop<'static, State>, display: &str) -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| anyhow::anyhow!("缺少 XDG_RUNTIME_DIR，无法创建 IPC 套接字"))?;
        let path = socket_path_in(&PathBuf::from(runtime), display);

        // 同名套接字只可能来自已退出的上一个实例。
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::debug!(path = %path.display(), "清理陈旧 IPC 套接字"),
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("清理旧 IPC 套接字失败：{}", path.display()));
            }
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("绑定 IPC 套接字失败：{}", path.display()))?;
        listener
            .set_nonblocking(true)
            .context("IPC 套接字设为非阻塞失败")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("设置 IPC 套接字权限失败")?;

        let handle = event_loop.handle();
        let listener_token = handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                |_, listener, state| {
                    state.ipc_accept(listener);
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|err| anyhow::anyhow!("注册 IPC 监听失败：{err}"))?;

        Ok(Self {
            path,
            handle,
            listener_token,
            clients: Vec::new(),
            next_id: 1,
            dispatching: None,
            drop_current: false,
            last_focus: None,
            last_tags: None,
            last_layout: None,
            last_output: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn register(&mut self, stream: UnixStream) {
        if let Err(err) = stream.set_nonblocking(true) {
            tracing::warn!("IPC 连接设为非阻塞失败：{err}");
            return;
        }
        let Ok(write_end) = stream.try_clone() else {
            tracing::warn!("复制 IPC 套接字失败");
            return;
        };

        let id = self.next_id;
        self.next_id += 1;

        match self.handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, state: &mut State| {
                // SAFETY: 只通过内部流读取，不替换/不移除它。
                let stream = unsafe { stream.get_mut() };
                Ok(state.ipc_client_event(id, stream))
            },
        ) {
            Ok(token) => {
                self.clients.push(Client {
                    id,
                    token,
                    stream: write_end,
                    read: Vec::new(),
                    topics: None,
                });
                tracing::debug!(id, "IPC 客户端接入");
            }
            Err(err) => tracing::warn!("注册 IPC 连接失败：{err}"),
        }
    }

    fn client_index(&self, id: u64) -> Option<usize> {
        self.clients.iter().position(|client| client.id == id)
    }

    fn client_mut(&mut self, id: u64) -> Option<&mut Client> {
        self.clients.iter_mut().find(|client| client.id == id)
    }

    /// 读走当前可读的字节，返回完整的请求行；`Err` 表示该断开。
    fn read_client(&mut self, id: u64, stream: &mut UnixStream) -> Result<Vec<String>, ()> {
        let Some(client) = self.client_mut(id) else {
            return Err(());
        };

        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return Err(()),
                Ok(read) => {
                    client.read.extend_from_slice(&chunk[..read]);
                    if client.read.len() > MAX_READ_BUFFER {
                        tracing::warn!(id, "IPC 请求缓冲溢出，断开");
                        return Err(());
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    tracing::debug!(id, %err, "IPC 读取失败");
                    return Err(());
                }
            }
        }

        let lines = split_lines(&client.read).map_err(|err| {
            tracing::warn!(id, %err, "IPC 请求不合法，断开");
        })?;
        if let Some(last) = client.read.iter().rposition(|byte| *byte == b'\n') {
            client.read.drain(..=last);
        }
        Ok(lines)
    }

    /// 立即写出一行；返回 `false` 表示积压/断连，调用方应断开该连接。
    fn write_client(&mut self, id: u64, bytes: &[u8]) -> bool {
        let Some(index) = self.client_index(id) else {
            return true;
        };

        let mut written = 0;
        while written < bytes.len() {
            match self.clients[index].stream.write(&bytes[written..]) {
                Ok(0) => return false,
                Ok(count) => written += count,
                Err(err) if err.kind() == ErrorKind::WouldBlock => return false,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    tracing::debug!(id, %err, "IPC 写入失败");
                    return false;
                }
            }
        }
        true
    }

    /// 发送一行；失败就断开（慢客户端不拖累合成器）。
    fn deliver(&mut self, id: u64, bytes: &[u8]) {
        if self.write_client(id, bytes) {
            return;
        }
        tracing::warn!(id, "IPC 发送积压或失败，断开慢客户端");
        self.drop_client(id);
    }

    fn drop_client(&mut self, id: u64) {
        if self.dispatching == Some(id) {
            // 正在这个连接的回调里：只移除表项，由回调返回 `PostAction::Remove`。
            self.drop_current = true;
            self.clients.retain(|client| client.id != id);
            return;
        }

        let Some(index) = self.client_index(id) else {
            return;
        };
        let client = self.clients.remove(index);
        self.handle.remove(client.token);
        tracing::debug!(id, "IPC 客户端断开");
    }

    /// 广播事件（只发给订阅了该主题的连接）。
    fn emit(&mut self, event: &Event) {
        let Ok(mut bytes) = serde_json::to_vec(event) else {
            return;
        };
        bytes.push(b'\n');

        let topic = match event {
            Event::Hello { .. } => return,
            Event::Focus { .. } => Topic::Focus,
            Event::Tag { .. } => Topic::Tag,
            Event::Title { .. } => Topic::Title,
            Event::Layout { .. } => Topic::Layout,
            Event::Output { .. } => Topic::Output,
        };

        let targets: Vec<u64> = self
            .clients
            .iter()
            .filter(|client| client.subscribed(topic))
            .map(|client| client.id)
            .collect();
        for id in targets {
            self.deliver(id, &bytes);
        }
    }

    /// 只发给某个连接（订阅时的初始快照）。
    fn send_to(&mut self, id: u64, event: &Event) {
        let Ok(mut bytes) = serde_json::to_vec(event) else {
            return;
        };
        bytes.push(b'\n');
        self.deliver(id, &bytes);
    }

    /// 去重后广播焦点/tag/布局变化。
    pub fn publish(&mut self, output: &str, focus: Option<u64>, tags: u32, layout: &str) {
        if self.last_focus != Some(focus) {
            self.last_focus = Some(focus);
            self.emit(&Event::Focus { window: focus });
        }
        if self.last_tags != Some(tags) {
            self.last_tags = Some(tags);
            self.emit(&Event::Tag {
                output: output.to_string(),
                tags,
            });
        }
        if self.last_layout.as_deref() != Some(layout) {
            self.last_layout = Some(layout.to_string());
            self.emit(&Event::Layout {
                output: output.to_string(),
                layout: layout.to_string(),
            });
        }
    }

    /// 标题变化是高频事件，只在真的变了时广播。
    pub fn publish_title(&mut self, id: u64, title: &str) {
        self.emit(&Event::Title {
            window: id,
            title: title.to_string(),
        });
    }

    /// 输出模式/缩放/工作区变化；内容一样就不重复发。
    pub fn publish_output(&mut self, event: Event) {
        if self.last_output.as_ref() == Some(&event) {
            return;
        }
        self.last_output = Some(event.clone());
        self.emit(&event);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.handle.remove(self.listener_token);
        for client in &self.clients {
            self.handle.remove(client.token);
        }
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %err, "清理 IPC 套接字失败");
        }
    }
}

/// 事件循环回调：把可读事件转给连接状态机。
impl State {
    /// 接受所有排队的 IPC 连接。
    pub(crate) fn ipc_accept(&mut self, listener: &UnixListener) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        loop {
            match listener.accept() {
                Ok((stream, _)) => ipc.register(stream),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    tracing::warn!("接受 IPC 连接失败：{err}");
                    break;
                }
            }
        }
    }

    pub(crate) fn ipc_client_event(&mut self, id: u64, stream: &mut UnixStream) -> PostAction {
        let mut lines = Vec::new();
        let mut dead = false;

        if let Some(ipc) = self.ipc.as_mut() {
            ipc.dispatching = Some(id);
            ipc.drop_current = false;
            match ipc.read_client(id, stream) {
                Ok(read) => lines = read,
                Err(()) => {
                    ipc.drop_client(id);
                    dead = true;
                }
            }
        }

        for line in lines {
            self.ipc_handle_line(id, &line);
        }

        let Some(ipc) = self.ipc.as_mut() else {
            return PostAction::Continue;
        };
        ipc.dispatching = None;
        dead |= ipc.drop_current;
        ipc.drop_current = false;

        if dead || ipc.client_index(id).is_none() {
            PostAction::Remove
        } else {
            PostAction::Continue
        }
    }

    fn ipc_handle_line(&mut self, id: u64, line: &str) {
        let request = match serde_json::from_str::<Request>(line) {
            Ok(request) => request,
            Err(err) => {
                self.ipc_reply(
                    id,
                    json!({"ok": false, "error": format!("请求解析失败：{err}")}),
                );
                self.ipc_close(id);
                return;
            }
        };

        match request {
            Request::Command { action, arg } => {
                match Action::parse(&action, arg.as_deref()) {
                    Ok(action) => {
                        self.execute(action);
                        self.ipc_reply(id, json!({"ok": true}));
                    }
                    Err(err) => self.ipc_reply(id, json!({"ok": false, "error": err.to_string()})),
                }
                // 命令/查询是「一问一答」：回完就关，客户端读到 EOF 即结束。
                self.ipc_close(id);
            }
            Request::Query { what } => {
                let reply = self.ipc_query(what);
                self.ipc_reply(id, reply);
                self.ipc_close(id);
            }
            Request::Subscribe { topics } => self.ipc_subscribe(id, topics),
        }
    }

    fn ipc_close(&mut self, id: u64) {
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.drop_client(id);
        }
    }

    fn ipc_reply(&mut self, id: u64, value: serde_json::Value) {
        let Ok(mut bytes) = serde_json::to_vec(&value) else {
            return;
        };
        bytes.push(b'\n');
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.deliver(id, &bytes);
        }
    }

    fn ipc_subscribe(&mut self, id: u64, topics: Vec<Topic>) {
        // 先算快照，再从 `self.ipc` 取可变引用，避免同时借用 `self`。
        let output = self.output_name();
        let focus = self.shell.focus();
        let tags = self.shell.visible_tags(&output);
        let layout = self
            .shell
            .layout_of(&output, self.shell.focused_tag(&output))
            .name()
            .to_string();
        let output_event = self.output_event();

        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        if let Some(client) = ipc.client_mut(id) {
            client.topics = Some(topics);
        } else {
            return;
        }

        ipc.send_to(
            id,
            &Event::Hello {
                version: PROTOCOL_VERSION,
            },
        );

        // 初始快照：订阅者不必再发查询就能画出一帧。
        ipc.send_to(id, &Event::Focus { window: focus });
        ipc.send_to(
            id,
            &Event::Tag {
                output: output.clone(),
                tags,
            },
        );
        ipc.send_to(id, &Event::Layout { output, layout });
        if let Some(event) = output_event {
            ipc.send_to(id, &event);
        }
    }

    fn ipc_query(&mut self, what: Query) -> serde_json::Value {
        match what {
            Query::Clients => {
                let focus = self.shell.focus();
                let clients: Vec<_> = self
                    .shell
                    .iter()
                    .map(|entry| {
                        let location = self.space.element_location(&entry.window);
                        let size = entry.window.geometry().size;
                        json!({
                            "id": entry.id,
                            "title": entry.title,
                            "app_id": entry.app_id,
                            "tags": entry.tags,
                            "sticky": entry.sticky,
                            "layer": layer_name(entry.layer),
                            "focused": focus == Some(entry.id),
                            "location": location.map(|point| json!({"x": point.x, "y": point.y})),
                            "size": {"w": size.w, "h": size.h},
                        })
                    })
                    .collect();
                json!({"ok": true, "query": "clients", "data": clients})
            }
            Query::Tags => {
                let output = self.output_name();
                let visible = self.shell.visible_tags(&output);
                let focused = self.shell.focused_tag(&output);
                let tags: Vec<_> = (1..=9u32)
                    .map(|number| {
                        let bit = 1u32 << (number - 1);
                        let windows = self
                            .shell
                            .iter()
                            .filter(|entry| entry.output == output && entry.tags & bit != 0)
                            .count();
                        json!({
                            "tag": number,
                            "visible": visible & bit != 0,
                            "focused": focused == bit,
                            "layout": self.shell.layout_of(&output, bit).name(),
                            "windows": windows,
                        })
                    })
                    .collect();
                json!({
                    "ok": true,
                    "query": "tags",
                    "data": {"visible": visible, "focused": focused, "tags": tags},
                })
            }
            Query::Outputs => {
                let outputs: Vec<_> = self
                    .outputs
                    .iter()
                    .map(|output| {
                        let mode = output.current_mode();
                        let work = self
                            .work_areas
                            .get(&output.name())
                            .copied()
                            .unwrap_or(self.work_area);
                        json!({
                            "name": output.name(),
                            "width": mode.map(|mode| mode.size.w).unwrap_or(0),
                            "height": mode.map(|mode| mode.size.h).unwrap_or(0),
                            "refresh": mode.map(|mode| mode.refresh).unwrap_or(0),
                            "scale": output.current_scale().fractional_scale(),
                            "work_area": {
                                "x": work.x,
                                "y": work.y,
                                "width": work.width,
                                "height": work.height,
                            },
                        })
                    })
                    .collect();
                json!({"ok": true, "query": "outputs", "data": outputs})
            }
            Query::Focused => {
                let focus = self.shell.focus();
                let output = self.output_name();
                json!({
                    "ok": true,
                    "query": "focused",
                    "data": {"window": focus, "tag": self.shell.focused_tag(&output)},
                })
            }
        }
    }

    /// 当前输出名；IPC 事件里用它区分多输出。
    pub fn output_name(&self) -> String {
        self.output
            .as_ref()
            .map(|output| output.name())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Tiled => "tiled",
        Layer::Floating => "floating",
        Layer::Fullscreen => "fullscreen",
    }
}

/// 纯函数：把缓冲里的完整行切出来，供 [`IpcServer::read_client`] 与单测复用。
fn split_lines(buffer: &[u8]) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(offset) = buffer[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + offset;
        let payload = &buffer[start..end];
        if payload.len() > MAX_REQUEST_LINE {
            return Err(format!("请求行超过 {MAX_REQUEST_LINE} 字节"));
        }
        let text = std::str::from_utf8(payload).map_err(|err| format!("非法 UTF-8：{err}"))?;
        let text = text.trim_end_matches('\r');
        if !text.is_empty() {
            lines.push(text.to_string());
        }
        start = end + 1;
    }
    if buffer.len() - start > MAX_REQUEST_LINE {
        return Err(format!("请求行超过 {MAX_REQUEST_LINE} 字节"));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_lines_splits_and_ignores_blank() {
        let lines = split_lines(b"a\n\nb\r\nc").unwrap();
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn take_lines_rejects_overlong_line() {
        let mut buffer = vec![b'x'; MAX_REQUEST_LINE + 1];
        buffer.push(b'\n');
        assert!(split_lines(&buffer).is_err());
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        assert!(split_lines(b"\xff\n").is_err());
    }
}
