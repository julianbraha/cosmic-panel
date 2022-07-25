// SPDX-License-Identifier: MPL-2.0-only

use std::{
    cell::Cell,
    ffi::OsString,
    fs,
    os::unix::{net::UnixStream, prelude::AsRawFd},
    rc::Rc,
    time::Instant,
};

use anyhow::bail;
use freedesktop_desktop_entry::{self, DesktopEntry, Iter};
use itertools::Itertools;
use sctk::{
    environment::Environment,
    output::OutputInfo,
    reexports::{
        client::protocol::{wl_output as c_wl_output, wl_surface as c_wl_surface},
        client::{self, Attached, Main},
        protocols::{
            wlr::unstable::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1},
            xdg_shell::client::{
                xdg_popup,
                xdg_positioner::{Anchor, Gravity, XdgPositioner},
                xdg_surface,
                xdg_wm_base::XdgWmBase,
            },
        },
    },
};
use slog::{info, trace, Logger};
use smithay::{
    backend::{
        renderer::gles2::Gles2Renderer,
    },
    desktop::{Kind, PopupKind, PopupManager, Space, Window, WindowSurfaceType},
    reexports::wayland_server::{protocol::wl_surface::WlSurface as s_WlSurface, DisplayHandle, self},
    utils::{Logical, Size},
    wayland::shell::xdg::{PopupSurface, PositionerState},
};
use xdg_shell_wrapper::{
    client_state::{Env, ClientFocus},
    config::WrapperConfig,
    space::{Popup, PopupState, SpaceEvent, Visibility, WrapperSpace},
    util::{exec_child, get_client_sock}, server_state::{ServerFocus, ServerPointerFocus}, output::c_output_as_s_output,
};

use cosmic_panel_config::{CosmicPanelConfig};

use super::PanelSpace;

impl WrapperSpace for PanelSpace {
    type Config = CosmicPanelConfig;

    fn handle_events(&mut self, _: &DisplayHandle, _: &mut PopupManager, _: u32) -> Instant {
        panic!("this should not be called");
    }


    fn add_window(&mut self, w: Window) {
        self.full_clear = 4;
        self.space.commit(&w.toplevel().wl_surface());
        self.space
            .map_window(&w, (0, 0), self.z_index().map(|z| z as u8), true);
        for w in self.space.windows() {
            w.configure();
        }
    }

    fn add_popup(
        &mut self,
        env: &Environment<Env>,
        xdg_wm_base: &Attached<XdgWmBase>,
        s_surface: PopupSurface,
        positioner: Main<XdgPositioner>,
        PositionerState {
            rect_size,
            anchor_rect,
            anchor_edges,
            gravity,
            constraint_adjustment,
            offset,
            reactive,
            parent_size,
            parent_configure: _,
        }: PositionerState,
    ) {
        // TODO handle popups not on main surface
        if !self.popups.is_empty() {
            self.popups.clear();
            return;
        }

        let parent_window = if let Some(s) = self.space.windows().find(|w| match w.toplevel() {
            Kind::Xdg(wl_s) => Some(wl_s.wl_surface()) == s_surface.get_parent_surface().as_ref(),
        }) {
            s
        } else {
            return;
        };

        let c_wl_surface = env.create_surface().detach();
        let c_xdg_surface = xdg_wm_base.get_xdg_surface(&c_wl_surface);

        let wl_surface = s_surface.wl_surface().clone();
        let s_popup_surface = s_surface.clone();

        let p_offset = self
            .space
            .window_location(parent_window)
            .unwrap_or_else(|| (0, 0).into());
        // dbg!(s.bbox().loc);
        positioner.set_size(rect_size.w, rect_size.h);
        positioner.set_anchor_rect(
            anchor_rect.loc.x + p_offset.x,
            anchor_rect.loc.y + p_offset.y,
            anchor_rect.size.w,
            anchor_rect.size.h,
        );
        positioner.set_anchor(Anchor::from_raw(anchor_edges as u32).unwrap_or(Anchor::None));
        positioner.set_gravity(Gravity::from_raw(gravity as u32).unwrap_or(Gravity::None));

        positioner.set_constraint_adjustment(u32::from(constraint_adjustment));
        positioner.set_offset(offset.x, offset.y);
        if positioner.as_ref().version() >= 3 {
            if reactive {
                positioner.set_reactive();
            }
            if let Some(parent_size) = parent_size {
                positioner.set_parent_size(parent_size.w, parent_size.h);
            }
        }
        let c_popup = c_xdg_surface.get_popup(None, &positioner);
        self.layer_surface.as_ref().unwrap().get_popup(&c_popup);

        //must be done after role is assigned as popup
        c_wl_surface.commit();

        let cur_popup_state = Rc::new(Cell::new(Some(PopupState::WaitConfigure(true))));
        c_xdg_surface.quick_assign(move |c_xdg_surface, e, _| {
            if let xdg_surface::Event::Configure { serial, .. } = e {
                c_xdg_surface.ack_configure(serial);
            }
        });

        let popup_state = cur_popup_state.clone();

        c_popup.quick_assign(move |_c_popup, e, _| {
            if let Some(PopupState::Closed) = popup_state.get().as_ref() {
                return;
            }

            match e {
                xdg_popup::Event::Configure {
                    x,
                    y,
                    width,
                    height,
                } => {
                    if popup_state.get() != Some(PopupState::Closed) {
                        let _ = s_popup_surface.send_configure();

                        let first = match popup_state.get() {
                            Some(PopupState::Configure { first, .. }) => first,
                            Some(PopupState::WaitConfigure(first)) => first,
                            _ => false,
                        };
                        popup_state.set(Some(PopupState::Configure {
                            first,
                            x,
                            y,
                            width,
                            height,
                        }));
                    }
                }
                xdg_popup::Event::PopupDone => {
                    popup_state.set(Some(PopupState::Closed));
                }
                xdg_popup::Event::Repositioned { token } => {
                    popup_state.set(Some(PopupState::Repositioned(token)));
                }
                _ => {}
            };
        });

        self.popups.push(Popup {
            c_popup,
            c_xdg_surface,
            c_wl_surface,
            s_surface,
            egl_surface: None,
            dirty: false,
            popup_state: cur_popup_state,
            position: (0, 0).into(),
            accumulated_damage: Default::default(),
            full_clear: 4,
        });
    }

    fn reposition_popup(
        &mut self,
        s_popup: PopupSurface,
        _: Main<XdgPositioner>,
        _: PositionerState,
        token: u32,
    ) -> anyhow::Result<()> {
        s_popup.send_repositioned(token);
        s_popup.send_configure()?;
        Ok(())
    }

    fn config(&self) -> Self::Config {
        self.config.clone()
    }

    fn spawn_clients(
        &mut self,
        mut display: DisplayHandle,
    ) -> Result<Vec<UnixStream>, anyhow::Error> {
        if self.children.is_empty() {
            let (clients_left, sockets_left): (Vec<_>, Vec<_>) = (0..self
                .config
                .plugins_left
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0))
                .map(|_p| {
                    let (c, s) = get_client_sock(&mut display);
                    (c, s)
                })
                .unzip();
            self.clients_left = clients_left;
            let (clients_center, sockets_center): (Vec<_>, Vec<_>) = (0..self
                .config
                .plugins_center
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0))
                .map(|_p| {
                    let (c, s) = get_client_sock(&mut display);
                    (c, s)
                })
                .unzip();
            self.clients_center = clients_center;
            let (clients_right, sockets_right): (Vec<_>, Vec<_>) = (0..self
                .config
                .plugins_right
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0))
                .map(|_p| {
                    let (c, s) = get_client_sock(&mut display);
                    (c, s)
                })
                .unzip();
            self.clients_right = clients_right;

            let mut desktop_ids = self
                .config
                .plugins_left
                .iter()
                .chain(self.config.plugins_center.iter())
                .chain(self.config.plugins_right.iter())
                .flatten()
                .zip(
                    sockets_left
                        .into_iter()
                        .chain(sockets_center.into_iter())
                        .chain(sockets_right.into_iter()),
                )
                .collect_vec();

                self.children = Iter::new(freedesktop_desktop_entry::default_paths())
                .filter_map(|path| {
                    if let Some(position) = desktop_ids.iter().position(|(app_file_name, _)| {
                        Some(OsString::from(app_file_name).as_os_str()) == path.file_stem()
                    }) {
                        // This way each applet is at most started once,
                        // even if multiple desktop files in different directories match
                        let (_, client_socket) = desktop_ids.remove(position);
                        fs::read_to_string(&path).ok().and_then(|bytes| {
                            if let Ok(entry) = DesktopEntry::decode(&path, &bytes) {
                                if let Some(exec) = entry.exec() {
                                    let requests_host_wayland_display =
                                        entry.desktop_entry("HostWaylandDisplay").is_some();
                                    return Some(exec_child(
                                        exec,
                                        Some(self.config.name()),
                                        self.log.clone(),
                                        client_socket.as_raw_fd(),
                                        requests_host_wayland_display,
                                    ));
                                }
                            }
                            None
                        })
                    } else {
                        None
                    }
                })
                .collect_vec();

            Ok(desktop_ids.into_iter().map(|(_, socket)| socket).collect())
        } else {
            bail!("Clients have already been spawned!");
        }
    }

    fn log(&self) -> Option<Logger> {
        Some(self.log.clone())
    }

    fn destroy(&mut self) {
        self.layer_surface.as_mut().map(|ls| ls.destroy());
        self.layer_shell_wl_surface
            .as_mut()
            .map(|wls| wls.destroy());
    }

    fn visibility(&self) -> Visibility {
        Visibility::Visible
    }

    fn raise_window(&mut self, w: &Window, activate: bool) {
        self.space.raise_window(w, activate);
    }

    fn dirty_window(&mut self, _dh: &DisplayHandle, s: &s_WlSurface) {
        self.last_dirty = Some(Instant::now());

        if let Some(w) = self.space.window_for_surface(s, WindowSurfaceType::ALL) {
            let old_bbox = w.bbox();
            self.space.commit(&s);
            w.refresh();
            let new_bbox = w.bbox();
            if old_bbox.size != new_bbox.size {
                self.full_clear = 4;
            }

            // TODO improve this for when there are changes to the lists of plugins while running
            let padding: Size<i32, Logical> = (
                (2 * self.config.padding()).try_into().unwrap(),
                (2 * self.config.padding()).try_into().unwrap(),
            )
                .into();
            let size = self.constrain_dim(padding + w.bbox().size);
            let pending_dimensions = self.pending_dimensions.unwrap_or(self.dimensions);
            let mut wait_configure_dim = self
                .space_event
                .get()
                .map(|e| match e {
                    SpaceEvent::Configure {
                        width,
                        height,
                        serial: _serial,
                        ..
                    } => (width, height),
                    SpaceEvent::WaitConfigure { width, height, .. } => (width, height),
                    _ => self.dimensions.into(),
                })
                .unwrap_or(pending_dimensions.into());
            if self.dimensions.w < size.w
                && pending_dimensions.w < size.w
                && wait_configure_dim.0 < size.w
            {
                self.pending_dimensions = Some((size.w, wait_configure_dim.1).into());
                wait_configure_dim.0 = size.w;
            }
            if self.dimensions.h < size.h
                && pending_dimensions.h < size.h
                && wait_configure_dim.1 < size.h
            {
                self.pending_dimensions = Some((wait_configure_dim.0, size.h).into());
            }
        }
    }

    fn dirty_popup(&mut self, dh: &DisplayHandle, s: &s_WlSurface) {
        self.space.commit(&s);
        self.space.refresh(&dh);
        if let Some(p) = self
            .popups
            .iter_mut()
            .find(|p| p.s_surface.wl_surface() == s)
        {
            p.dirty = true;
        }
    }

    // XXX the renderer is provided by the container, not tracked by the PanelSpace
    fn renderer(&mut self) -> Option<&mut Gles2Renderer> {
        None
    }

    fn setup(
        &mut self,
        _: wayland_server::DisplayHandle,
        env: &Environment<Env>,
        c_display: client::Display,
        c_focused_surface: ClientFocus,
        c_hovered_surface: ClientFocus,
    ) {
        let layer_shell = env.require_global::<zwlr_layer_shell_v1::ZwlrLayerShellV1>();
        let pool = env
            .create_auto_pool()
            .expect("Failed to create a memory pool!");

        self.layer_shell.replace(layer_shell);
        self.pool.replace(pool);
        self.c_focused_surface = c_focused_surface;
        self.c_hovered_surface = c_hovered_surface;
        self.c_display.replace(c_display);
    }

    fn handle_output(
        &mut self,
        _: wayland_server::DisplayHandle,
        env: &Environment<Env>,
        output: Option<&c_wl_output::WlOutput>,
        output_info: Option<&OutputInfo>,
    ) -> anyhow::Result<()> {
        if let Some(info) = output_info {
            if info.obsolete {
                self.space_event.replace(Some(SpaceEvent::Quit));
            }
        }

        self.output = output.cloned().zip(output_info.cloned());
        let c_surface = env.create_surface();
        let dimensions = self.constrain_dim((1, 1).into());
        let layer_surface = self.layer_shell.as_ref().unwrap().get_layer_surface(
            &c_surface,
            output,
            self.config.layer(),
            "".to_owned(),
        );

        layer_surface.set_anchor(self.config.anchor.into());
        layer_surface.set_keyboard_interactivity(self.config.keyboard_interactivity());
        layer_surface.set_size(
            dimensions.w.try_into().unwrap(),
            dimensions.h.try_into().unwrap(),
        );

        // Commit so that the server will send a configure event
        c_surface.commit();

        let next_render_event = Rc::new(Cell::new(Some(SpaceEvent::WaitConfigure {
            first: true,
            width: dimensions.w,
            height: dimensions.h,
        })));

        let next_render_event_handle = next_render_event.clone();
        let logger = self.log.clone();
        layer_surface.quick_assign(move |layer_surface, event, _| {
            match (event, next_render_event_handle.get()) {
                (zwlr_layer_surface_v1::Event::Closed, _) => {
                    info!(logger, "Received close event. closing.");
                    next_render_event_handle.set(Some(SpaceEvent::Quit));
                }
                (
                    zwlr_layer_surface_v1::Event::Configure {
                        serial,
                        width,
                        height,
                    },
                    next,
                ) if next != Some(SpaceEvent::Quit) => {
                    trace!(
                        logger,
                        "received configure event {:?} {:?} {:?}",
                        serial,
                        width,
                        height
                    );
                    layer_surface.ack_configure(serial);

                    let first = match next {
                        Some(SpaceEvent::Configure { first, .. }) => first,
                        Some(SpaceEvent::WaitConfigure { first, .. }) => first,
                        _ => false,
                    };
                    next_render_event_handle.set(Some(SpaceEvent::Configure {
                        first,
                        width: if width == 0 {
                            dimensions.w
                        } else {
                            width.try_into().unwrap()
                        },
                        height: if height == 0 {
                            dimensions.h
                        } else {
                            height.try_into().unwrap()
                        },
                        serial: serial.try_into().unwrap(),
                    }));
                }
                (_, _) => {}
            }
        });

        self.layer_surface.replace(layer_surface);
        self.dimensions = dimensions;
        self.space_event = next_render_event;
        self.full_clear = 4;
        self.layer_shell_wl_surface = Some(c_surface);
        Ok(())
    }

    /// returns false to forward the button press, and true to intercept
    fn handle_press(&mut self, seat_name: &str) -> Option<s_WlSurface> {

        let prev_foc = {
            let c_hovered_surface = self.c_hovered_surface.borrow_mut();

            match c_hovered_surface.iter().enumerate().find(|(i, f)| f.1 == seat_name) {
                Some((i, f)) => (i, f.0.clone()),
                None => return None,
            }
        };

        if **self.layer_shell_wl_surface.as_ref().unwrap() == prev_foc.1
            && !self.popups.is_empty()
        {
            self.close_popups();
        } else {
        }
        self.s_hovered_surface.iter().find_map(|h| {
            if h.seat_name.as_str() == seat_name {
                Some(h.surface.clone())
            } else {
                None
            }
        })
    }

    ///  update active window based on pointer location
    fn update_pointer(&mut self, (x, y): (i32, i32), seat_name: &str) -> Option<ServerPointerFocus> {
        let mut prev_foc = self.s_hovered_surface.iter_mut().enumerate().find(|(i, f)| f.seat_name == seat_name);

        // set new focused
        if let Some((_, s, _)) = self
            .space
            .surface_under((x as f64, y as f64), WindowSurfaceType::ALL)
        {
            if let Some((_, prev_foc)) = prev_foc.as_mut() {
                prev_foc.surface = s.clone();
                Some(prev_foc.clone())
            } else {
                self.s_hovered_surface.push(ServerPointerFocus { surface: s, seat_name: seat_name.to_string(), c_pos: (0, 0).into(), s_pos: (x, y).into()}); // TODO better c_pos
                self.s_hovered_surface.last().cloned()
            }
        } else {
            if let Some((prev_i, _)) = prev_foc {
                self.s_hovered_surface.swap_remove(prev_i);
            }
            None
        }
    }

    fn keyboard_leave(&mut self, seat_name: &str, surface: Option<c_wl_surface::WlSurface>) {
        let prev_len = self.s_focused_surface.len();
        self.s_focused_surface.retain(|(_, name)| {
            name != name
        });
        if prev_len != self.s_focused_surface.len() {
            self.close_popups();
        }
    }

    fn keyboard_enter(&mut self, seat_name: &str, surface: c_wl_surface::WlSurface) -> Option<s_WlSurface> {
        //  anything to do here that isn't done already by handle button press?
        None
    }

    fn pointer_leave(&mut self, seat_name: &str, surface: Option<c_wl_surface::WlSurface>) {        
        self.s_hovered_surface.retain(|focus| {
            focus.seat_name != seat_name
        });
    }

    fn pointer_enter(&mut self, seat_name: &str, surface: c_wl_surface::WlSurface) {
        // anything to do here that isn't done already by update pointer?
    }
}