# ADR 0009 — 通知：独立 D-Bus 线程 + calloop channel

## 状态
已采纳（2026-11-03）

## 背景
基线 g0wm 自己实现 `org.freedesktop.Notifications`（`src/notify.c`，约 1000 行 C 与
libdbus），因此 bar 上的通知不依赖 mako/dunst。r0wm 的定位同样要求「单二进制自足」
（ADR 0001），不能让外部通知守护补位。

D-Bus 的 Rust 生态以 async 为主（zbus）。合成器的渲染/输入跑在 calloop 单线程里，
直接引入 tokio 或把 async 任务塞进 calloop 会让主循环多一层调度，并且 `Notify`
方法需要在回复里**同步**给出 id，与「先丢给渲染线程、以后再回」的模型不匹配。

## 决策
1. 通知状态机 `notify::Store` 保持纯数据：不依赖 D-Bus、Smithay，时间由调用者以
   单调毫秒传入，因此过期/hold/抢占都能单测。
2. `org.freedesktop.Notifications` 实现放独立线程，用 zbus 的 blocking 连接
   （内部自带 async-io 执行器）。名字已被 mako/dunst 占用时不抢，只告警并停掉服务。
3. 两个方向都走消息：
   - 合成器 → 服务：`std::sync::mpsc` 的 `NotifyHandle`（dismiss / invoke / hold /
     history remove / setTimeout）；
   - 服务 → 合成器：calloop channel 推 `Snapshot`（当前通知、排队数、历史、剩余
     毫秒），bar 只读快照，不共享锁。
4. 信号（`NotificationClosed` / `ActionInvoked` / `ActivationToken`）先排队，由服务
   线程统一发出；`ActivationToken` 排在对应 `ActionInvoked` 之前（规范要求）。

## 权衡
- 在 calloop 里直接驱动 zbus async：少一个线程，但要改主循环调度模型，且接口方法
  与状态机在同一线程，`Notify` 的同步回复要额外做 oneshot 往返。
- 用 libdbus 绑定（与基线同构）：C 依赖与 unsafe 面积大，违反 ADR 0002 的「无 C
  依赖」。
- 独立线程 + 快照：多一份拷贝（历史最多 32 条、图标统一 64px），换来渲染线程无锁、
  无 async、D-Bus 卡住也不会拖住帧循环；代价是信号与 UI 之间有一帧级延迟，可接受。
