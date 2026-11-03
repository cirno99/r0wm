//! 通知：`org.freedesktop.Notifications` 的状态机（纯数据，无 D-Bus / Smithay 依赖）。
//!
//! 对标基线 `src/notify.c`：**屏幕上一次只显示一条**，其余排队，关掉的进一小段
//! 历史。这里只负责「哪条在显示、什么时候过期、该发什么信号」，D-Bus 与绘制
//! 分别在 [`service`] 和 `ui::bar` 里。时间由调用者以单调毫秒传入，单测因此
//! 可以精确控制过期与 hold。
//!
//! 基线的几条硬规则在这里原样保留：
//!
//! - `expire_timeout <= 0` 用服务端默认值，`> 60s` 截到 60s；critical 永不自动过期；
//! - 同 id、或同 app + stack tag 的通知原地更新，排队中的也一样；
//! - 取消（`CloseNotification`）的不进历史，其余关闭都进；
//! - transient 的通知既不进历史也不占位。

mod service;

pub use service::{NotifyHandle, start as start_service};

use std::collections::VecDeque;
use std::sync::Arc;

/// 正文最长字节数（含结尾，基线 `NOTIFY_TEXTMAX`）。
pub const TEXT_MAX: usize = 512;
/// 排队上限（基线 `NOTIFY_QUEUEMAX`）。
pub const QUEUE_MAX: usize = 32;
/// 历史上限（基线 `NOTIFY_HISTMAX`）。
pub const HIST_MAX: usize = 32;
/// 除 `default` 外的动作上限（基线 `NOTIFY_ACTMAX`）。
pub const ACT_MAX: usize = 8;
/// 服务端强制的最长超时，防止一条通知永久占住 bar（基线 `NOTIFY_TIMEOUT_MAX`）。
pub const TIMEOUT_MAX_MS: u32 = 60_000;
/// 图标边长的解码上限；超过就拒收，不做缩放（基线 `NOTIFY_IMAGEMAX`）。
pub const ICON_SIDE_MAX: i32 = 4096;
/// 图标在内存里统一缩到这个边长，历史再多也不会爆（基线 `NOTIFY_ICONSIZE`）。
pub const ICON_SIZE: i32 = 64;

/// 紧急程度，数字与规范一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    pub fn from_hint(value: u8) -> Self {
        match value {
            0 => Self::Low,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }
}

/// 一条动作：`key` 原样回给客户端，`label` 用于显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub key: String,
    pub label: String,
}

/// 图标类型住在 `ui::icon`（runner、systray 也要用），这里只做转出。
pub use crate::ui::icon::Icon;

/// 通知关闭原因，数字即 `NotificationClosed` 的 reason。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    ByCall = 3,
    Undefined = 4,
}

impl CloseReason {
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

/// 状态机产生的副作用；D-Bus 层负责把它们变成信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Closed { id: u32, reason: CloseReason },
    ActionInvoked { id: u32, key: String },
}

/// 客户端 `Notify` 的参数（提示已解析成具体字段）。
#[derive(Debug, Clone, Default)]
pub struct Incoming {
    pub replaces_id: u32,
    pub app: String,
    pub summary: String,
    /// body 原文（可能带 `<b>` 之类的标记）。
    pub body: String,
    pub urgency: Urgency,
    pub transient: bool,
    pub resident: bool,
    /// 客户端给的毫秒数，原样传入（`<= 0` 表示用默认值）。
    pub timeout_ms: i32,
    pub actions: Vec<Action>,
    /// actions 列表里是否有 `default`。
    pub has_default: bool,
    pub desktop: String,
    pub tag: String,
    /// 进度 0-100。
    pub value: Option<u8>,
    pub icon: Option<Arc<Icon>>,
}

/// 正在显示或排队的一条通知。
#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    pub urgency: Urgency,
    pub has_default: bool,
    pub resident: bool,
    /// 是否上过屏（决定 critical 的抢占顺序）。
    pub seen: bool,
    pub transient: bool,
    pub timeout_ms: u32,
    /// 上屏时刻（单调毫秒）。
    pub shown_ms: u64,
    pub text: String,
    pub app: String,
    pub desktop: String,
    pub tag: String,
    pub value: Option<u8>,
    pub actions: Vec<Action>,
    pub icon: Option<Arc<Icon>>,
}

impl Notification {
    /// 同 id，或同 app + stack tag；排队更新靠它找槽位。
    fn same_slot(&self, other: &Self) -> bool {
        self.id == other.id
            || (!self.tag.is_empty() && self.tag == other.tag && self.app == other.app)
    }
}

/// 给 bar/合成器的只读快照；整体相等就不重画。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub current: Option<Item>,
    pub queued: usize,
    /// 最新一条在前。
    pub history: Vec<Item>,
}

/// 快照里的一条（动作只留标签，图标共享）。
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: u32,
    pub text: String,
    pub app: String,
    pub desktop: String,
    pub urgency: Urgency,
    pub timeout_ms: u32,
    /// 当前还剩多少毫秒（0 表示不过期）。
    pub left_ms: u32,
    pub has_default: bool,
    pub actions: Vec<String>,
    pub value: Option<u8>,
    pub icon: Option<Arc<Icon>>,
}

/// 通知状态机。
pub struct Store {
    /// 分给客户端的 id，从 1 开始；0 表示「没有」。
    seq: u32,
    active: Option<Notification>,
    queue: VecDeque<Notification>,
    /// 最新一条在前。
    history: VecDeque<Notification>,
    /// 用户正在挑动作：计时暂停，新通知只排队。
    held: bool,
    /// 暂停时剩下的毫秒数。
    held_left: u32,
    default_timeout_ms: u32,
    events: Vec<Event>,
}

impl Store {
    pub fn new(default_timeout_secs: u32) -> Self {
        let mut store = Self {
            seq: 0,
            active: None,
            queue: VecDeque::new(),
            history: VecDeque::new(),
            held: false,
            held_left: 0,
            default_timeout_ms: 1000,
            events: Vec::new(),
        };
        store.set_timeout_secs(default_timeout_secs);
        store
    }

    /// 基线 `notify_settimeout`：0 秒按 1 秒算，超过 60 秒截到 60 秒。
    pub fn set_timeout_secs(&mut self, seconds: u32) {
        let seconds = if seconds == 0 { 1 } else { seconds };
        self.default_timeout_ms = (seconds * 1000).min(TIMEOUT_MAX_MS);
    }

    pub fn default_timeout_ms(&self) -> u32 {
        self.default_timeout_ms
    }

    pub fn current(&self) -> Option<&Notification> {
        self.active.as_ref()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn history(&self) -> &VecDeque<Notification> {
        &self.history
    }

    pub fn is_held(&self) -> bool {
        self.held
    }

    /// 取走本轮的副作用（信号）。
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// 处理一次 `Notify`，返回分配/沿用到的 id。
    pub fn notify(&mut self, now: u64, incoming: Incoming) -> u32 {
        let mut n = Notification {
            id: 0,
            urgency: incoming.urgency,
            has_default: incoming.has_default,
            resident: incoming.resident,
            seen: false,
            transient: incoming.transient,
            timeout_ms: 0,
            shown_ms: 0,
            text: compose_text(&incoming.app, &incoming.summary, &incoming.body),
            app: sanitize(&incoming.app),
            desktop: sanitize(&incoming.desktop),
            tag: sanitize(&incoming.tag),
            value: incoming.value,
            actions: incoming.actions,
            icon: incoming.icon,
        };

        // -1 要默认值，0 在基线里同样落到默认值：bar 只有一格，不允许常驻。
        let ms = if incoming.timeout_ms <= 0 {
            self.default_timeout_ms
        } else {
            (incoming.timeout_ms as u32).min(TIMEOUT_MAX_MS)
        };
        n.timeout_ms = if n.urgency == Urgency::Critical {
            0
        } else {
            ms
        };

        // 更新：先按 replaces_id，再按 stack tag 找同槽位。
        let mut id = incoming.replaces_id;
        if id == 0 && !n.tag.is_empty() {
            if self.active.as_ref().is_some_and(|cur| cur.same_slot(&n)) {
                id = self.active.as_ref().unwrap().id;
            }
            if id == 0
                && let Some(found) = self.queue.iter().find(|cur| cur.same_slot(&n))
            {
                id = found.id;
            }
        }
        if id == 0 {
            self.seq = self.seq.wrapping_add(1);
            if self.seq == 0 {
                self.seq = self.seq.wrapping_add(1);
            }
            id = self.seq;
        }
        n.id = id;

        if self.active.as_ref().is_some_and(|cur| cur.id == id) {
            // 同一条更新：重置计时，等价于重新上屏。
            self.active = Some(n);
            self.show_active(now);
        } else if let Some(slot) = self.queue.iter().position(|cur| cur.id == id) {
            self.queue[slot] = n;
        } else if self.held {
            // 用户正在挑动作：任何通知都只排队，critical 排最前。
            self.enqueue(n, incoming.urgency == Urgency::Critical);
        } else {
            let active_expires = self.active.as_ref().is_some_and(|cur| cur.timeout_ms != 0);
            if self.active.is_none() || !active_expires || incoming.urgency == Urgency::Critical {
                // 新通知立刻上屏，被顶掉的那条回到队首。
                if let Some(cur) = self.active.take() {
                    self.enqueue(cur, true);
                }
                self.active = Some(n);
                self.show_active(now);
            } else {
                self.enqueue(n, false);
            }
        }
        id
    }

    /// 自动过期；服务线程在截止时刻调用。
    pub fn tick(&mut self, now: u64) {
        let expired = self.active.as_ref().is_some_and(|cur| cur.timeout_ms != 0)
            && !self.held
            && self
                .active
                .as_ref()
                .is_some_and(|cur| now >= cur.shown_ms + cur.timeout_ms as u64);
        if expired {
            self.clear(now, CloseReason::Expired);
        }
    }

    /// 客户端 `CloseNotification`。
    pub fn close(&mut self, now: u64, id: u32) -> bool {
        if self.active.as_ref().is_some_and(|cur| cur.id == id) {
            self.clear(now, CloseReason::ByCall);
            return true;
        }
        if let Some(slot) = self.queue.iter().position(|cur| cur.id == id) {
            self.queue.remove(slot);
            self.events.push(Event::Closed {
                id,
                reason: CloseReason::ByCall,
            });
            return true;
        }
        false
    }

    /// 用户关掉当前这条，队列里的下一条接上。
    pub fn dismiss(&mut self, now: u64) {
        self.clear(now, CloseReason::Dismissed);
    }

    /// 当前与排队的全部关掉，都进历史。
    pub fn dismiss_all(&mut self, now: u64) {
        let _ = now;
        if self.active.is_none() {
            return;
        }
        self.held = false;
        let cur = self.active.take().unwrap();
        self.events.push(Event::Closed {
            id: cur.id,
            reason: CloseReason::Dismissed,
        });
        self.hist_push(cur);
        while let Some(n) = self.queue.pop_front() {
            self.events.push(Event::Closed {
                id: n.id,
                reason: CloseReason::Dismissed,
            });
            self.hist_push(n);
        }
    }

    /// 触发 `default` 动作；没有就当作关掉（基线 `notify_invoke`）。
    pub fn invoke(&mut self, now: u64) {
        let Some(cur) = self.active.as_ref() else {
            return;
        };
        if cur.has_default {
            self.invoke_key(now, "default");
        } else {
            self.clear(now, CloseReason::Dismissed);
        }
    }

    /// 触发第 `index` 个动作。
    pub fn action(&mut self, now: u64, index: usize) {
        let Some(key) = self
            .active
            .as_ref()
            .and_then(|cur| cur.actions.get(index))
            .map(|action| action.key.clone())
        else {
            return;
        };
        self.invoke_key(now, &key);
    }

    /// 挑动作时暂停/恢复计时（基线 `notify_hold`）。
    pub fn hold(&mut self, now: u64, on: bool) {
        if self.active.is_none() || on == self.held {
            return;
        }
        if on {
            let cur = self.active.as_ref().unwrap();
            self.held_left = if cur.timeout_ms == 0 {
                0
            } else {
                (cur.shown_ms + cur.timeout_ms as u64)
                    .saturating_sub(now)
                    .max(1) as u32
            };
            self.held = true;
            return;
        }

        self.held = false;
        // 暂停期间来的、还没露过面的 critical 立刻抢占。
        if let Some(slot) = self
            .queue
            .iter()
            .position(|cur| cur.urgency == Urgency::Critical && !cur.seen)
            && self
                .active
                .as_ref()
                .is_some_and(|cur| cur.urgency != Urgency::Critical)
        {
            let n = self.queue.remove(slot).unwrap();
            let old = self.active.replace(n).unwrap();
            self.queue.push_front(old);
            self.show_active(now);
            return;
        }

        if let Some(cur) = self.active.as_mut()
            && cur.timeout_ms != 0
        {
            cur.shown_ms = now.saturating_sub(cur.timeout_ms as u64 - self.held_left as u64);
        }
    }

    /// 从历史里删一条（基线 `notify_histremove`）。
    pub fn history_remove(&mut self, id: u32) -> bool {
        let Some(slot) = self.history.iter().position(|cur| cur.id == id) else {
            return false;
        };
        self.history.remove(slot);
        true
    }

    /// 当前这条还剩多少毫秒；0 表示不过期。
    pub fn remaining_ms(&self, now: u64) -> u32 {
        let Some(cur) = self.active.as_ref() else {
            return 0;
        };
        if cur.timeout_ms == 0 {
            return 0;
        }
        if self.held {
            return self.held_left;
        }
        (cur.shown_ms + cur.timeout_ms as u64)
            .saturating_sub(now)
            .min(u32::MAX as u64) as u32
    }

    /// 下一次需要唤醒的绝对时刻（单调毫秒）。
    pub fn deadline(&self) -> Option<u64> {
        let cur = self.active.as_ref()?;
        (cur.timeout_ms != 0 && !self.held).then(|| cur.shown_ms + cur.timeout_ms as u64)
    }

    /// 只读快照；`now` 只影响 `left_ms`。
    pub fn snapshot(&self, now: u64) -> Snapshot {
        Snapshot {
            current: self.active.as_ref().map(|cur| self.item(cur, now)),
            queued: self.queue.len(),
            history: self.history.iter().map(|cur| self.item(cur, 0)).collect(),
        }
    }

    fn item(&self, cur: &Notification, now: u64) -> Item {
        let left_ms = if cur.timeout_ms == 0 {
            0
        } else if self.held && self.active.as_ref().is_some_and(|a| a.id == cur.id) {
            self.held_left
        } else {
            (cur.shown_ms + cur.timeout_ms as u64)
                .saturating_sub(now)
                .min(u32::MAX as u64) as u32
        };
        Item {
            id: cur.id,
            text: cur.text.clone(),
            app: cur.app.clone(),
            desktop: cur.desktop.clone(),
            urgency: cur.urgency,
            timeout_ms: cur.timeout_ms,
            left_ms,
            has_default: cur.has_default,
            actions: cur.actions.iter().map(|a| a.label.clone()).collect(),
            value: cur.value,
            icon: cur.icon.clone(),
        }
    }

    /// 让 `active` 上屏：记时间、标记见过。hold 期间保留剩余时间。
    fn show_active(&mut self, now: u64) {
        let Some(cur) = self.active.as_mut() else {
            return;
        };
        cur.shown_ms = now;
        cur.seen = true;
        if self.held {
            self.held_left = cur.timeout_ms;
        }
    }

    fn clear(&mut self, now: u64, reason: CloseReason) {
        let Some(cur) = self.active.take() else {
            return;
        };
        self.held = false;
        self.events.push(Event::Closed { id: cur.id, reason });
        // 客户端主动取消的不值得留。
        if reason != CloseReason::ByCall {
            self.hist_push(cur);
        }
        self.show_next(now);
    }

    fn show_next(&mut self, now: u64) {
        while self.active.is_none() {
            // 没露过面的 critical 优先；其次按队列顺序，但跳过常驻项。
            let slot = self
                .queue
                .iter()
                .position(|cur| cur.urgency == Urgency::Critical && !cur.seen)
                .or_else(|| self.queue.iter().position(|cur| cur.timeout_ms != 0))
                .or(if self.queue.is_empty() { None } else { Some(0) });
            let Some(slot) = slot else {
                break;
            };
            let n = self.queue.remove(slot).unwrap();
            self.active = Some(n);
            self.show_active(now);
        }
    }

    fn enqueue(&mut self, n: Notification, front: bool) {
        if self.queue.len() >= QUEUE_MAX {
            // 队列满：优先把最早的可过期条目直接送进历史。
            let slot = self
                .queue
                .iter()
                .position(|cur| cur.timeout_ms != 0)
                .unwrap_or(0);
            let old = self.queue.remove(slot).unwrap();
            self.events.push(Event::Closed {
                id: old.id,
                reason: CloseReason::Undefined,
            });
            self.hist_push(old);
        }
        if front {
            self.queue.push_front(n);
        } else {
            self.queue.push_back(n);
        }
    }

    /// 历史去重（同槽位替换），满了丢最旧，最新在前。
    fn hist_push(&mut self, n: Notification) {
        if n.transient {
            return;
        }
        if let Some(slot) = self.history.iter().position(|cur| cur.same_slot(&n)) {
            self.history.remove(slot);
        }
        if self.history.len() >= HIST_MAX {
            self.history.pop_back();
        }
        self.history.push_front(n);
    }

    fn invoke_key(&mut self, now: u64, key: &str) {
        let Some(cur) = self.active.as_ref() else {
            return;
        };
        self.events.push(Event::ActionInvoked {
            id: cur.id,
            key: key.to_string(),
        });
        if cur.resident {
            self.hold(now, false);
        } else {
            self.clear(now, CloseReason::Dismissed);
        }
    }
}

/// 去掉控制字符（D-Bus 字符串本身已是合法 UTF-8）。
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// 把 `app: summary - body` 拼成一行，正文先按规范允许的标记摊平。
pub(crate) fn compose_text(app: &str, summary: &str, body: &str) -> String {
    let app = sanitize(if app.is_empty() { "?" } else { app });
    let summary = sanitize(summary);
    let body = sanitize(&demarkup(body));
    let text = if !summary.is_empty() && !body.is_empty() {
        format!("{app}: {summary} - {body}")
    } else {
        let tail = if summary.is_empty() { &body } else { &summary };
        format!("{app}: {tail}")
    };
    truncate_utf8(&text, TEXT_MAX - 1)
}

/// 按字节上限截断，只保留完整码点。
fn truncate_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// 把正文标记摊平成纯文本：实体解码，规范允许的标签丢掉（`<br>` 变空格），
/// 其余 `<` 原样保留（基线 `demarkup`）。
pub fn demarkup(src: &str) -> String {
    const TAGS: [&str; 6] = ["b", "i", "u", "a", "img", "br"];
    const ENTS: [(&str, char); 5] = [
        ("amp;", '&'),
        ("lt;", '<'),
        ("gt;", '>'),
        ("quot;", '"'),
        ("apos;", '\''),
    ];

    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &src[i..];
        if let Some(tail) = rest.strip_prefix('&') {
            let mut decoded = false;
            for (name, c) in ENTS {
                if let Some(after) = tail.strip_prefix(name) {
                    out.push(c);
                    i = bytes.len() - after.len();
                    decoded = true;
                    break;
                }
            }
            if decoded {
                continue;
            }
            if let Some(after) = tail.strip_prefix('#') {
                let (radix, digits) = match after.strip_prefix(['x', 'X']) {
                    Some(hex) => (16, hex),
                    None => (10, after),
                };
                let end = digits
                    .find(|c: char| !c.is_digit(radix))
                    .unwrap_or(digits.len());
                if digits[end..].starts_with(';')
                    && let Ok(cp) = u32::from_str_radix(&digits[..end], radix)
                    && let Some(c) = char::from_u32(cp)
                {
                    out.push(c);
                    i = bytes.len() - digits[end + 1..].len();
                    continue;
                }
            }
        } else if let Some(gt) = rest
            .strip_prefix('<')
            .and_then(|tail| tail.find('>'))
            .map(|index| index + 1)
        {
            // 只认识规范列出的标签；大小写不敏感，允许带属性。
            let inner = &rest[1..gt];
            let name = inner.strip_prefix('/').unwrap_or(inner);
            let name = name
                .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .next()
                .unwrap_or("");
            if TAGS.iter().any(|tag| tag.eq_ignore_ascii_case(name)) {
                if name.eq_ignore_ascii_case("br") {
                    out.push(' ');
                }
                i += gt + 1;
                continue;
            }
        }
        let c = src[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(app: &str, summary: &str) -> Incoming {
        Incoming {
            app: app.into(),
            summary: summary.into(),
            ..Default::default()
        }
    }

    fn ids(store: &Store) -> Vec<(u32, String)> {
        store
            .history()
            .iter()
            .map(|n| (n.id, n.text.clone()))
            .collect()
    }

    #[test]
    fn timeout_defaults_cap_and_critical_stick() {
        let mut store = Store::new(5);
        let mut n = incoming("a", "one");
        n.timeout_ms = -1;
        let id = store.notify(0, n);
        assert_eq!(store.current().unwrap().id, id);
        assert_eq!(store.current().unwrap().timeout_ms, 5000);

        store.dismiss(10);
        let mut n = incoming("a", "two");
        n.timeout_ms = 120_000;
        store.notify(20, n);
        assert_eq!(store.current().unwrap().timeout_ms, TIMEOUT_MAX_MS);

        let mut n = incoming("a", "three");
        n.urgency = Urgency::Critical;
        n.timeout_ms = 5000;
        store.notify(30, n);
        assert_eq!(store.current().unwrap().timeout_ms, 0);
        assert_eq!(store.deadline(), None);
    }

    #[test]
    fn replace_by_id_or_stack_tag_updates_in_place() {
        let mut store = Store::new(5);
        let mut n = incoming("vol", "10%");
        n.tag = "volume".into();
        let id = store.notify(0, n);

        // 同 app + stack tag，不带 replaces_id 也原地更新。
        let mut n = incoming("vol", "30%");
        n.tag = "volume".into();
        assert_eq!(store.notify(0, n), id);
        assert_eq!(store.current().unwrap().text, "vol: 30%");

        // replaces_id 优先。
        let mut n = incoming("vol", "20%");
        n.replaces_id = id;
        assert_eq!(store.notify(0, n), id);
        assert_eq!(store.current().unwrap().text, "vol: 20%");

        // 同 tag 但不同 app 不合并。
        let mut n = incoming("other", "x");
        n.tag = "volume".into();
        let other = store.notify(0, n);
        assert_ne!(other, id);
    }

    #[test]
    fn queued_notifications_wait_for_a_free_slot() {
        let mut store = Store::new(5);
        store.notify(0, incoming("a", "first"));
        store.notify(0, incoming("b", "second"));
        assert_eq!(store.current().unwrap().text, "a: first");
        assert_eq!(store.queued(), 1);

        store.dismiss(1000);
        assert_eq!(store.current().unwrap().text, "b: second");
        assert_eq!(store.queued(), 0);
        // 被用户关掉的那条进历史，最新在前。
        assert_eq!(ids(&store)[0].1, "a: first");
    }

    #[test]
    fn critical_preempts_and_goes_first_when_held() {
        let mut store = Store::new(5);
        store.notify(0, incoming("a", "normal"));
        store.hold(100, true);
        let mut n = incoming("b", "critical");
        n.urgency = Urgency::Critical;
        store.notify(200, n);
        // hold 期间不换屏。
        assert_eq!(store.current().unwrap().text, "a: normal");
        assert_eq!(store.queued(), 1);

        store.hold(300, false);
        assert_eq!(store.current().unwrap().text, "b: critical");
        assert_eq!(store.queued(), 1);
    }

    #[test]
    fn expiry_closes_with_reason_and_shows_next() {
        let mut store = Store::new(5);
        store.notify(0, incoming("a", "first"));
        store.notify(0, incoming("b", "second"));
        assert_eq!(store.remaining_ms(4000), 1000);
        store.tick(4999);
        assert_eq!(store.current().unwrap().text, "a: first");
        store.tick(5000);
        assert_eq!(store.current().unwrap().text, "b: second");
        match store.drain_events().first() {
            Some(Event::Closed { reason, .. }) => assert_eq!(*reason, CloseReason::Expired),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn hold_freezes_the_countdown_and_resumes_it() {
        let mut store = Store::new(5);
        store.notify(0, incoming("a", "one"));
        store.hold(2000, true);
        assert_eq!(store.remaining_ms(10_000), 3000);
        assert_eq!(store.deadline(), None);
        store.hold(10_000, false);
        assert_eq!(store.remaining_ms(10_000), 3000);
        assert_eq!(store.deadline(), Some(13_000));
    }

    #[test]
    fn close_from_client_skips_history_but_dismiss_keeps_it() {
        let mut store = Store::new(5);
        let id = store.notify(0, incoming("a", "one"));
        assert!(store.close(10, id));
        assert!(store.history().is_empty());
        assert!(matches!(
            store.drain_events().as_slice(),
            [Event::Closed {
                reason: CloseReason::ByCall,
                ..
            }]
        ));

        let id = store.notify(20, incoming("a", "two"));
        store.dismiss(30);
        assert_eq!(store.history()[0].id, id);
    }

    #[test]
    fn transient_notifications_do_not_enter_history() {
        let mut store = Store::new(5);
        let mut n = incoming("a", "pop");
        n.transient = true;
        store.notify(0, n);
        store.dismiss(10);
        assert!(store.history().is_empty());
    }

    #[test]
    fn history_dedups_by_slot_and_keeps_newest_first() {
        let mut store = Store::new(5);
        let mut n = incoming("a", "one");
        n.tag = "tag".into();
        store.notify(0, n);
        store.dismiss(10);
        store.notify(20, incoming("b", "two"));
        store.dismiss(30);
        assert_eq!(ids(&store)[0].1, "b: two");

        // 同 app + tag 的历史条目被替换，而不是新增一条。
        let mut again = incoming("a", "one again");
        again.tag = "tag".into();
        store.notify(40, again);
        store.dismiss(50);
        assert_eq!(store.history().len(), 2);
        assert_eq!(ids(&store)[0].1, "a: one again");
        assert_eq!(ids(&store)[1].1, "b: two");
    }

    #[test]
    fn queue_overflow_moves_the_oldest_expiring_one_to_history() {
        let mut store = Store::new(5);
        // 常驻（critical）先上屏；第一条普通通知把它顶进队首。
        let mut n = incoming("a", "sticky");
        n.urgency = Urgency::Critical;
        store.notify(0, n);
        for i in 0..QUEUE_MAX {
            store.notify(0, incoming("q", &format!("{i}")));
        }
        assert_eq!(store.queued(), QUEUE_MAX);
        assert_eq!(store.current().unwrap().text, "q: 0");
        // 再塞一条普通的：队列里最早的可过期条目（q: 1）被挤进历史。
        store.notify(0, incoming("q", "overflow"));
        assert_eq!(store.queued(), QUEUE_MAX);
        assert_eq!(store.history()[0].text, "q: 1");
    }

    #[test]
    fn actions_invoke_then_close_unless_resident() {
        let mut store = Store::new(5);
        let mut n = incoming("app", "hi");
        n.has_default = true;
        n.actions = vec![Action {
            key: "open".into(),
            label: "Open".into(),
        }];
        store.notify(0, n);
        // 非 resident：发 ActionInvoked 后按 Dismissed 关闭。
        store.action(10, 0);
        let events = store.drain_events();
        assert!(matches!(
            events.as_slice(),
            [
                Event::ActionInvoked { key, .. },
                Event::Closed {
                    reason: CloseReason::Dismissed,
                    ..
                }
            ] if key == "open"
        ));
        assert!(store.current().is_none());

        let mut n = incoming("app", "sticky");
        n.has_default = true;
        n.resident = true;
        store.notify(20, n);
        store.invoke(30);
        assert!(matches!(
            store.drain_events().as_slice(),
            [Event::ActionInvoked { key, .. }] if key == "default"
        ));
        assert!(store.current().is_some());
    }

    #[test]
    fn dismiss_all_empties_everything_into_history() {
        let mut store = Store::new(5);
        store.notify(0, incoming("a", "one"));
        store.notify(0, incoming("b", "two"));
        store.notify(0, incoming("c", "three"));
        store.dismiss_all(10);
        assert!(store.current().is_none());
        assert_eq!(store.queued(), 0);
        assert_eq!(store.history().len(), 3);
        // 最新的在前：c 最后入历史。
        assert_eq!(ids(&store)[0].1, "c: three");
    }

    #[test]
    fn snapshot_reports_left_time_and_labels() {
        let mut store = Store::new(10);
        let mut n = incoming("app", "hi");
        n.actions = vec![Action {
            key: "k".into(),
            label: "Label".into(),
        }];
        n.value = Some(42);
        store.notify(1000, n);
        store.notify(1000, incoming("b", "queued"));
        let snap = store.snapshot(4000);
        let cur = snap.current.unwrap();
        assert_eq!(cur.left_ms, 7000);
        assert_eq!(cur.actions, vec!["Label".to_string()]);
        assert_eq!(cur.value, Some(42));
        assert_eq!(snap.queued, 1);
    }

    #[test]
    fn text_composition_and_demarkup() {
        assert_eq!(compose_text("foot", "", "plain"), "foot: plain".to_string());
        assert_eq!(
            compose_text("app", "summary", "body"),
            "app: summary - body"
        );
        assert_eq!(compose_text("", "x", ""), "?: x");
        assert_eq!(
            demarkup("<b>bold</b> &amp; <i>it</i><br>next"),
            "bold & it next"
        );
        assert_eq!(demarkup("a <b c=1>keep</b>"), "a keep");
        assert_eq!(demarkup("x &#65; &#x42; &lt;"), "x A B <");
        assert_eq!(
            demarkup("<unknown>kept</unknown>"),
            "<unknown>kept</unknown>"
        );
    }

    #[test]
    fn control_characters_become_spaces_and_text_is_truncated() {
        assert_eq!(sanitize("a\tb\u{7f}c"), "a b c");
        let long = "x".repeat(TEXT_MAX * 2);
        let text = compose_text("app", &long, "");
        assert!(text.len() < TEXT_MAX);
        assert!(text.starts_with("app: xxx"));
    }
}
