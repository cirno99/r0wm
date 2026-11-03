//! 键盘焦点目标：Wayland surface 与 XWayland 的 X11 surface。
//!
//! 指针焦点仍然直接用 `WlSurface`：XWayland 会把指针事件转给对应的
//! X11 surface，`X11Surface` 的 `PointerTarget` 也只是转发到它自己的
//! wl_surface。键盘不一样——X11 窗口需要 X 服务器真正拿到 input focus，
//! 所以必须把 `X11Surface` 作为焦点目标交给 seat。

use std::borrow::Cow;

use smithay::{
    backend::input::{InputTime, KeyState},
    desktop::PopupKind,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
};

#[cfg(feature = "xwayland")]
use smithay::xwayland::X11Surface;

use super::state::State;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Wayland(WlSurface),
    #[cfg(feature = "xwayland")]
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    /// 解析到对应的 Wayland surface；data-device 焦点按客户端设置时要用。
    pub fn wl_surface(&self) -> Option<WlSurface> {
        match self {
            Self::Wayland(surface) => Some(surface.clone()),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => surface.wl_surface(),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => surface.alive(),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => surface.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => surface.wl_surface().map(Cow::Owned),
        }
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        target
            .wl_surface()
            .expect("焦点目标没有 wl_surface（X11 窗口尚未关联）")
    }
}

impl KeyboardTarget<State> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => KeyboardTarget::leave(surface, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: InputTime,
    ) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => KeyboardTarget::modifiers(surface, seat, data, modifiers, serial),
        }
    }
}
