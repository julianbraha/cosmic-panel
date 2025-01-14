use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs,
    os::{fd::OwnedFd, unix::prelude::AsRawFd},
    rc::Rc,
    time::Instant,
};

use anyhow::bail;
use cosmic_panel_config::{CosmicPanelConfig, CosmicPanelOuput, NAME};
use freedesktop_desktop_entry::{self, DesktopEntry, Iter};
use itertools::izip;
use launch_pad::process::Process;
use sctk::{
    compositor::{CompositorState, Region},
    output::OutputInfo,
    reexports::client::{
        protocol::{wl_output as c_wl_output, wl_surface as c_wl_surface},
        Connection, Proxy, QueueHandle,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface, LayerSurfaceConfigure,
        },
        xdg::popup,
        WaylandSurface,
    },
};
use shlex::Shlex;
use smithay::{
    backend::renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
    desktop::{utils::bbox_from_surface_tree, PopupKind, PopupManager, Window},
    output::Output,
    reexports::wayland_server::{
        self, protocol::wl_surface::WlSurface as s_WlSurface, DisplayHandle, Resource,
    },
    utils::{Logical, Rectangle, Size},
    wayland::{
        compositor::{with_states, SurfaceAttributes},
        seat::WaylandFocus,
        shell::xdg::{PopupSurface, PositionerState, SurfaceCachedState},
    },
};
use smithay::{desktop::space::SpaceElement, wayland::fractional_scale::with_fractional_scale};
use tokio::sync::oneshot;
use tracing::{error, error_span, info, info_span, trace};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;
use xdg_shell_wrapper::{
    client_state::ClientFocus,
    server_state::ServerPointerFocus,
    shared_state::GlobalState,
    space::{SpaceEvent, Visibility, WrapperPopup, WrapperPopupState, WrapperSpace},
    util::get_client_sock,
    wp_fractional_scaling::FractionalScalingManager,
    wp_security_context::{SecurityContext, SecurityContextManager},
    wp_viewporter::ViewporterState,
};

use crate::space::AppletMsg;

use super::PanelSpace;

impl WrapperSpace for PanelSpace {
    type Config = CosmicPanelConfig;

    /// set the display handle of the space
    fn set_display_handle(&mut self, s_display: wayland_server::DisplayHandle) {
        self.s_display.replace(s_display);
    }

    /// get the client hovered surface of the space
    fn get_client_hovered_surface(&self) -> Rc<RefCell<ClientFocus>> {
        self.c_hovered_surface.clone()
    }

    /// get the client focused surface of the space
    fn get_client_focused_surface(&self) -> Rc<RefCell<ClientFocus>> {
        self.c_focused_surface.clone()
    }

    fn add_window(&mut self, w: Window) {
        self.is_dirty = true;
        if let Some(s) = w.wl_surface() {
            with_states(&s, |states| {
                with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(self.scale);
                });
            });
        }
        self.space.map_element(w.clone(), (0, 0), false);
    }

    fn add_popup<W: WrapperSpace>(
        &mut self,
        compositor_state: &sctk::compositor::CompositorState,
        fractional_scale_manager: Option<&FractionalScalingManager<W>>,
        viewport: Option<&ViewporterState<W>>,
        _conn: &sctk::reexports::client::Connection,
        qh: &QueueHandle<GlobalState<W>>,
        xdg_shell_state: &mut sctk::shell::xdg::XdgShell,
        s_surface: PopupSurface,
        positioner: sctk::shell::xdg::XdgPositioner,
        positioner_state: PositionerState,
    ) -> anyhow::Result<()> {
        self.apply_positioner_state(&positioner, positioner_state, &s_surface);
        // TODO handle popups not on main surface
        if !self.popups.is_empty() {
            self.popups.clear();
            return Ok(());
        }

        let c_wl_surface = compositor_state.create_surface(qh);

        let c_popup = popup::Popup::from_surface(
            None,
            &positioner,
            qh,
            c_wl_surface.clone(),
            xdg_shell_state,
        )?;

        let input_region = Region::new(compositor_state)?;

        if let (Some(s_window_geometry), Some(input_regions)) =
            with_states(s_surface.wl_surface(), |states| {
                (
                    states.cached_state.current::<SurfaceCachedState>().geometry,
                    states
                        .cached_state
                        .current::<SurfaceAttributes>()
                        .input_region
                        .as_ref()
                        .cloned(),
                )
            })
        {
            c_popup.xdg_surface().set_window_geometry(
                s_window_geometry.loc.x,
                s_window_geometry.loc.y,
                s_window_geometry.size.w.max(1),
                s_window_geometry.size.h.max(1),
            );
            for r in input_regions.rects {
                input_region.add(0, 0, r.1.size.w, r.1.size.h);
            }
            c_wl_surface.set_input_region(Some(input_region.wl_region()));
        }

        self.layer.as_ref().unwrap().get_popup(c_popup.xdg_popup());

        let fractional_scale =
            fractional_scale_manager.map(|f| f.fractional_scaling(&c_wl_surface, &qh));

        let viewport = viewport.map(|v| {
            with_states(&s_surface.wl_surface(), |states| {
                with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(self.scale);
                });
            });
            let viewport = v.get_viewport(&c_wl_surface, &qh);
            viewport.set_destination(
                positioner_state.rect_size.w.max(1),
                positioner_state.rect_size.h.max(1),
            );
            viewport
        });
        if fractional_scale.is_none() {
            c_wl_surface.set_buffer_scale(self.scale as i32);
        }

        // //must be done after role is assigned as popup
        c_wl_surface.commit();

        let cur_popup_state = Some(WrapperPopupState::WaitConfigure);

        self.popups.push(WrapperPopup {
            damage_tracked_renderer: OutputDamageTracker::new(
                positioner_state
                    .rect_size
                    .to_f64()
                    .to_physical(self.scale)
                    .to_i32_round(),
                1.0,
                smithay::utils::Transform::Flipped180,
            ),
            c_popup,
            s_surface,
            egl_surface: None,
            dirty: false,
            rectangle: Rectangle::from_loc_and_size((0, 0), positioner_state.rect_size),
            state: cur_popup_state,
            input_region,
            wrapper_rectangle: Rectangle::from_loc_and_size((0, 0), positioner_state.rect_size),
            positioner,
            has_frame: true,
            fractional_scale,
            viewport,
            scale: self.scale,
        });

        Ok(())
    }

    fn reposition_popup(
        &mut self,
        popup: PopupSurface,
        pos_state: PositionerState,
        token: u32,
    ) -> anyhow::Result<()> {
        popup.with_pending_state(|pending| {
            pending.geometry = Rectangle::from_loc_and_size((0, 0), pos_state.rect_size);
        });
        if let Some(p) = self.popups.iter().find(|wp| &wp.s_surface == &popup) {
            let positioner = &p.positioner;
            p.c_popup.xdg_surface().set_window_geometry(
                0,
                0,
                pos_state.rect_size.w.max(1),
                pos_state.rect_size.h.max(1),
            );
            self.apply_positioner_state(&positioner, pos_state, &p.s_surface);

            if positioner.version() >= 3 {
                p.c_popup.reposition(&positioner, token);
            }
            p.c_popup.wl_surface().commit();
            if positioner.version() >= 3 {
                popup.send_repositioned(token);
            }
        }

        popup.send_configure()?;
        Ok(())
    }

    fn config(&self) -> Self::Config {
        self.config.clone()
    }

    fn spawn_clients<W: WrapperSpace>(
        &mut self,
        mut display: DisplayHandle,
        qh: &QueueHandle<GlobalState<W>>,
        security_context_manager: Option<SecurityContextManager>,
    ) -> anyhow::Result<()> {
        info!("Spawning applets");
        let mut left_guard = self.clients_left.lock().unwrap();
        let mut center_guard = self.clients_center.lock().unwrap();
        let mut right_guard = self.clients_right.lock().unwrap();

        if left_guard.is_empty() && center_guard.is_empty() && right_guard.is_empty() {
            *left_guard = self
                .config
                .plugins_left()
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|id| {
                    let (c, s) = get_client_sock(&mut display);
                    (id, c, Some(s), None)
                })
                .collect();

            *center_guard = self
                .config
                .plugins_center()
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|id| {
                    let (c, s) = get_client_sock(&mut display);
                    (id, c, Some(s), None)
                })
                .collect();

            *right_guard = self
                .config
                .plugins_right()
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|id| {
                    let (c, s) = get_client_sock(&mut display);
                    (id, c, Some(s), None)
                })
                .collect();

            let mut desktop_ids: Vec<_> = left_guard
                .iter_mut()
                .map(|(a, b, c, d)| (a, b, c, d, self.clients_left.clone()))
                .chain(
                    center_guard
                        .iter_mut()
                        .map(|(a, b, c, d)| (a, b, c, d, self.clients_center.clone())),
                )
                .chain(
                    right_guard
                        .iter_mut()
                        .map(|(a, b, c, d)| (a, b, c, d, self.clients_right.clone())),
                )
                .collect();

            let config_size = ron::ser::to_string(&self.config.size).unwrap_or_default();
            let active_output = self
                .output
                .as_ref()
                .and_then(|o| o.2.name.clone())
                .unwrap_or_default();

            let config_anchor = ron::ser::to_string(&self.config.anchor).unwrap_or_default();
            let config_bg = ron::ser::to_string(&self.config.background).unwrap_or_default();
            let env_vars = vec![
                ("COSMIC_PANEL_SIZE".to_string(), config_size),
                ("COSMIC_PANEL_OUTPUT".to_string(), active_output),
                ("COSMIC_PANEL_ANCHOR".to_string(), config_anchor),
                ("COSMIC_PANEL_BACKGROUND".to_string(), config_bg),
                ("RUST_BACKTRACE".to_string(), "1".to_string()),
            ];
            info!("{:?}", &desktop_ids);

            for path in Iter::new(freedesktop_desktop_entry::default_paths()) {
                if let Some(position) = desktop_ids.iter().position(|(app_file_name, ..)| {
                    Some(OsString::from(app_file_name).as_os_str()) == path.file_stem()
                }) {
                    // This way each applet is at most started once,
                    // even if multiple desktop files in different directories match
                    let (id, client, socket, applet_security_context, my_list) =
                        desktop_ids.remove(position);
                    info!(id);

                    let Some(socket) = socket.take() else {
                        error!("Failed to get socket for {}", &id);
                        continue;
                    };

                    if let Ok(bytes) = fs::read_to_string(&path) {
                        if let Ok(entry) = DesktopEntry::decode(&path, &bytes) {
                            if let Some(exec) = entry.exec() {
                                info!("Starting: {}", exec);

                                let requests_wayland_display =
                                    entry.desktop_entry("X-HostWaylandDisplay").is_some();

                                let mut exec_iter = Shlex::new(exec);
                                let exec = exec_iter
                                    .next()
                                    .expect("exec parameter must contain at least on word");

                                let mut args = Vec::new();
                                for arg in exec_iter {
                                    trace!("child argument: {}", &arg);
                                    args.push(arg);
                                }
                                let mut fds = Vec::with_capacity(2);
                                let mut applet_env = Vec::new();

                                if requests_wayland_display {
                                    if let Some(security_context_manager) =
                                        security_context_manager.as_ref()
                                    {
                                        match security_context_manager.create_listener::<W>(qh) {
                                            Ok(security_context) => {
                                                security_context
                                                    .set_sandbox_engine(NAME.to_string());
                                                security_context.commit();

                                                let data = security_context
                                                    .data::<SecurityContext>()
                                                    .unwrap();
                                                let privileged_socket =
                                                    data.conn.lock().unwrap().take().unwrap();
                                                applet_env.push((
                                                    "X_PRIVILEGED_WAYLAND_SOCKET".to_string(),
                                                    privileged_socket.as_raw_fd().to_string(),
                                                ));
                                                fds.push(privileged_socket.into());
                                                *applet_security_context = Some(security_context);
                                            }
                                            Err(why) => {
                                                error!(?why, "Failed to create a listener");
                                            }
                                        }
                                    };
                                }

                                for (key, val) in &env_vars {
                                    if !requests_wayland_display && *key == "WAYLAND_DISPLAY" {
                                        continue;
                                    }
                                    applet_env.push((key.clone(), val.clone()));
                                }
                                applet_env.push((
                                    "WAYLAND_SOCKET".to_string(),
                                    socket.as_raw_fd().to_string(),
                                ));

                                fds.push(socket.into());
                                trace!("child: {}, {:?} {:?}", &exec, args, applet_env);
                                let is_notification_applet =
                                    entry.desktop_entry("X-NotificationsApplet").is_some();

                                let display_handle = display.clone();
                                let applet_tx_clone = self.applet_tx.clone();
                                let id_clone = id.clone();
                                let id_clone_info = id.clone();
                                let id_clone_err = id.clone();
                                let client_id = client.id();
                                let client_id_info = client.id();
                                let client_id_err = client.id();
                                let security_context_manager_clone =
                                    security_context_manager.clone();
                                let qh_clone = qh.clone();

                                let mut process = Process::new()
                                    .with_executable(&exec)
                                    .with_args(args)
                                    .with_on_stderr(move |_, _, out| {
                                        // TODO why is span not included in logs to journald
                                        let id_clone = id_clone_err.clone();
                                        let client_id = client_id_err.clone();

                                        async move {
                                            error_span!("stderr", client = ?client_id).in_scope(
                                                || {
                                                    error!("{}: {}", id_clone, out);
                                                },
                                            );
                                        }
                                    })
                                    .with_on_stdout(move |_, _, out| {
                                        let id_clone = id_clone_info.clone();
                                        let client_id = client_id_info.clone();
                                        // TODO why is span not included in logs to journald
                                        async move {
                                            info_span!("stdout", client = ?client_id).in_scope(
                                                || {
                                                    info!("{}: {}", id_clone, out);
                                                },
                                            );
                                        }
                                    })
                                    .with_on_exit(move |mut pman, key, err_code, is_restarting| {
                                        let my_list = my_list.clone();
                                        let mut display_handle = display_handle.clone();
                                        let id_clone = id_clone.clone();
                                        let applet_tx_clone = applet_tx_clone.clone();
                                        let (c, client_socket) =
                                            get_client_sock(&mut display_handle);
                                        let raw_client_socket = client_socket.as_raw_fd();
                                        let client_id_clone = client_id.clone();
                                        let mut applet_env = Vec::with_capacity(1);
                                        let mut fds: Vec<OwnedFd> = Vec::with_capacity(2);
                                        let security_context = if requests_wayland_display {
                                            security_context_manager_clone.as_ref().and_then(
                                                |security_context_manager| {
                                                    security_context_manager
                                                        .create_listener(&qh_clone)
                                                        .ok()
                                                        .map(|security_context| {
                                                            security_context.set_sandbox_engine(
                                                                NAME.to_string(),
                                                            );
                                                            security_context.commit();

                                                            let data = security_context
                                                                .data::<SecurityContext>()
                                                                .unwrap();
                                                            let privileged_socket = data
                                                                .conn
                                                                .lock()
                                                                .unwrap()
                                                                .take()
                                                                .unwrap();
                                                            applet_env.push((
                                                                "X_PRIVILEGED_WAYLAND_SOCKET"
                                                                    .to_string(),
                                                                privileged_socket
                                                                    .as_raw_fd()
                                                                    .to_string(),
                                                            ));
                                                            fds.push(privileged_socket.into());
                                                            security_context
                                                        })
                                                },
                                            )
                                        } else {
                                            None
                                        };

                                        async move {
                                            if let Some(err_code) = err_code {
                                                error!("Exited with error code {}", err_code)
                                            }
                                            if !is_restarting {
                                                return;
                                            }

                                            if is_notification_applet {
                                                let (tx, rx) = oneshot::channel();
                                                _ = applet_tx_clone
                                                    .send(AppletMsg::NeedNewNotificationFd(tx))
                                                    .await;
                                                let Ok(fd) = rx.await else {
                                                    error!("Failed to get new fd");
                                                    return;
                                                };
                                                if let Err(err) = pman
                                                    .update_process_env(
                                                        &key,
                                                        vec![(
                                                            "COSMIC_NOTIFICATIONS".to_string(),
                                                            fd.as_raw_fd().to_string(),
                                                        )],
                                                    )
                                                    .await
                                                {
                                                    error!("Failed to update process env: {}", err);
                                                    return;
                                                }
                                                fds.push(fd);
                                                fds.push(client_socket.into());
                                                if let Err(err) =
                                                    pman.update_process_fds(&key, move || fds).await
                                                {
                                                    error!("Failed to update process fds: {}", err);
                                                    return;
                                                }
                                            } else {
                                                fds.push(client_socket.into());
                                                if let Err(err) =
                                                    pman.update_process_fds(&key, move || fds).await
                                                {
                                                    error!("Failed to update process fds: {}", err);
                                                    return;
                                                }
                                            }

                                            if let Some(old_client) = my_list
                                                .lock()
                                                .unwrap()
                                                .iter_mut()
                                                .find(|(id, _, _, _)| id == &id_clone)
                                            {
                                                old_client.1 = c;
                                                old_client.3 = security_context;
                                                info!("Replaced the client socket");
                                            } else {
                                                error!(
                                                    "Failed to find matching client... {}",
                                                    &id_clone
                                                )
                                            }
                                            let _ = applet_tx_clone
                                                .send(AppletMsg::ClientSocketPair(client_id_clone))
                                                .await;
                                            applet_env.push((
                                                "WAYLAND_SOCKET".to_string(),
                                                raw_client_socket.to_string(),
                                            ));
                                            let _ = pman
                                                .update_process_env(&key, applet_env.clone())
                                                .await;
                                        }
                                    });

                                let process_id = format!("{}-{}", self.id(), id);
                                let msg = if is_notification_applet {
                                    AppletMsg::NewNotificationsProcess(
                                        process_id, process, applet_env, fds,
                                    )
                                } else {
                                    process = process.with_fds(move || fds);

                                    AppletMsg::NewProcess(process_id, process.with_env(applet_env))
                                };
                                match self.applet_tx.try_send(msg) {
                                    Ok(_) => {}
                                    Err(e) => error!("{e}"),
                                };
                            }
                        }
                    }
                }
            }

            info!("Done spawning applets");
            Ok(())
        } else {
            anyhow::bail!("Clients have already been spawned!");
        }
    }

    fn destroy(&mut self) {
        unimplemented!()
    }

    fn visibility(&self) -> Visibility {
        self.visibility
    }

    fn raise_window(&mut self, w: &Window, activate: bool) {
        self.space.raise_element(w, activate);
    }

    fn dirty_window(&mut self, _dh: &DisplayHandle, s: &s_WlSurface) {
        self.is_dirty = true;
        self.last_dirty = Some(Instant::now());
        if let Some(w) = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_ref() == Some(s))
        {
            w.on_commit();
            w.refresh();
        }
    }

    fn dirty_popup(&mut self, _dh: &DisplayHandle, s: &s_WlSurface) {
        self.is_dirty = true;
        self.space.refresh();

        if let Some(p) = self
            .popups
            .iter_mut()
            .find(|p| p.s_surface.wl_surface() == s)
        {
            let p_bbox = bbox_from_surface_tree(p.s_surface.wl_surface(), (0, 0));
            let p_geo = PopupKind::Xdg(p.s_surface.clone()).geometry();
            if p_bbox != p.rectangle && p_bbox.size.w > 0 && p_bbox.size.h > 0 {
                p.c_popup.xdg_surface().set_window_geometry(
                    p_geo.loc.x,
                    p_geo.loc.y,
                    p_geo.size.w.max(1),
                    p_geo.size.h.max(1),
                );
                if let Some(input_regions) = with_states(p.s_surface.wl_surface(), |states| {
                    states
                        .cached_state
                        .current::<SurfaceAttributes>()
                        .input_region
                        .as_ref()
                        .cloned()
                }) {
                    p.input_region.subtract(
                        p_bbox.loc.x,
                        p_bbox.loc.y,
                        p_bbox.size.w,
                        p_bbox.size.h,
                    );
                    for r in input_regions.rects {
                        p.input_region.add(0, 0, r.1.size.w, r.1.size.h);
                    }
                    p.c_popup
                        .wl_surface()
                        .set_input_region(Some(p.input_region.wl_region()));
                }

                p.state = Some(WrapperPopupState::Rectangle {
                    x: p_bbox.loc.x,
                    y: p_bbox.loc.y,
                    width: p_bbox.size.w.max(1),
                    height: p_bbox.size.h.max(1),
                });
            }
            p.dirty = true;
        }
    }

    // XXX the renderer is provided by the container, not tracked by the PanelSpace
    fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        unimplemented!()
    }

    fn setup<W: WrapperSpace>(
        &mut self,
        _compositor_state: &CompositorState,
        _fractional_scale_manager: Option<&FractionalScalingManager<W>>,
        _security_context_manager: Option<SecurityContextManager>,
        _viewport: Option<&ViewporterState<W>>,
        _layer_state: &mut LayerShell,
        _conn: &Connection,
        _qh: &QueueHandle<GlobalState<W>>,
    ) {
    }

    /// returns false to forward the button press, and true to intercept
    fn handle_press(&mut self, seat_name: &str) -> Option<s_WlSurface> {
        if let Some(prev_foc) = {
            let c_hovered_surface: &ClientFocus = &self.c_hovered_surface.borrow();

            c_hovered_surface
                .iter()
                .enumerate()
                .find(|(_, f)| f.1 == seat_name)
                .map(|(i, f)| (i, f.0.clone()))
        } {
            // close popups when panel is pressed
            if self.layer.as_ref().map(|s| s.wl_surface()) == Some(&prev_foc.1)
                && !self.popups.is_empty()
            {
                self.close_popups();
            }
            self.s_hovered_surface.iter().find_map(|h| {
                if h.seat_name.as_str() == seat_name {
                    Some(h.surface.clone())
                } else {
                    None
                }
            })
        } else {
            // no hover found
            // if has keyboard focus remove it and close popups
            self.keyboard_leave(seat_name, None);
            None
        }
    }

    ///  update active window based on pointer location
    fn update_pointer(
        &mut self,
        (x, y): (i32, i32),
        seat_name: &str,
        c_wl_surface: c_wl_surface::WlSurface,
    ) -> Option<ServerPointerFocus> {
        let mut prev_hover = self
            .s_hovered_surface
            .iter_mut()
            .enumerate()
            .find(|(_, f)| f.seat_name == seat_name);
        let prev_foc = self.s_focused_surface.iter_mut().find(|f| f.1 == seat_name);
        // first check if the motion is on a popup's client surface
        if let Some(p) = self
            .popups
            .iter()
            .find(|p| p.c_popup.wl_surface() == &c_wl_surface)
        {
            let geo = smithay::desktop::PopupKind::Xdg(p.s_surface.clone()).geometry();
            // special handling for popup bc they exist on their own client surface

            if let Some(prev_foc) = prev_foc {
                prev_foc.0 = p.s_surface.wl_surface().clone();
            } else {
                self.s_focused_surface
                    .push((p.s_surface.wl_surface().clone(), seat_name.to_string()));
            }
            if let Some((_, prev_foc)) = prev_hover.as_mut() {
                prev_foc.c_pos = p.rectangle.loc;
                prev_foc.s_pos = p.rectangle.loc - geo.loc;

                prev_foc.surface = p.s_surface.wl_surface().clone();
                Some(prev_foc.clone())
            } else {
                self.s_hovered_surface.push(ServerPointerFocus {
                    surface: p.s_surface.wl_surface().clone(),
                    seat_name: seat_name.to_string(),
                    c_pos: p.rectangle.loc,
                    s_pos: p.rectangle.loc - geo.loc,
                });
                self.s_hovered_surface.last().cloned()
            }
        } else {
            // if not on this panel's client surface return None
            if self
                .layer
                .as_ref()
                .map(|s| *s.wl_surface() != c_wl_surface)
                .unwrap_or(true)
            {
                if self.space.elements().any(|e| {
                    e.wl_surface()
                        .zip(prev_hover.as_ref())
                        .map(|(s, prev_hover)| s == prev_hover.1.surface)
                        .unwrap_or_default()
                }) {
                    let (pos, _) = prev_hover.unwrap();
                    self.s_hovered_surface.remove(pos);
                }
                return None;
            }
            if let Some((w, relative_loc)) = self.space.element_under((x as f64, y as f64)) {
                // XXX HACK
                let geo = w
                    .bbox()
                    .to_f64()
                    .to_physical(1.0)
                    .to_logical(self.scale)
                    .to_i32_round();
                if let Some(prev_kbd) = prev_foc {
                    prev_kbd.0 = w.toplevel().wl_surface().clone();
                } else {
                    self.s_focused_surface
                        .push((w.toplevel().wl_surface().clone(), seat_name.to_string()));
                }
                if let Some((_, prev_foc)) = prev_hover.as_mut() {
                    prev_foc.s_pos = relative_loc;
                    prev_foc.c_pos = geo.loc;
                    prev_foc.surface = w.wl_surface().unwrap();
                    Some(prev_foc.clone())
                } else {
                    self.s_hovered_surface.push(ServerPointerFocus {
                        surface: w.wl_surface().unwrap(),
                        seat_name: seat_name.to_string(),
                        c_pos: geo.loc,
                        s_pos: relative_loc,
                    });
                    self.s_hovered_surface.last().cloned()
                }
            } else {
                if let Some((prev_i, _)) = prev_hover {
                    self.s_hovered_surface.swap_remove(prev_i);
                }
                None
            }
        }
    }

    fn keyboard_leave(&mut self, seat_name: &str, _: Option<c_wl_surface::WlSurface>) {
        let prev_len = self.s_focused_surface.len();
        self.s_focused_surface.retain(|(_, name)| name != seat_name);

        if prev_len != self.s_focused_surface.len() {
            self.close_popups();
        }
    }

    fn keyboard_enter(&mut self, _: &str, _: c_wl_surface::WlSurface) -> Option<s_WlSurface> {
        None
    }

    fn pointer_leave(&mut self, seat_name: &str, _: Option<c_wl_surface::WlSurface>) {
        self.s_hovered_surface
            .retain(|focus| focus.seat_name != seat_name);
    }

    fn pointer_enter(
        &mut self,
        dim: (i32, i32),
        seat_name: &str,
        c_wl_surface: c_wl_surface::WlSurface,
    ) -> Option<ServerPointerFocus> {
        self.update_pointer(dim, seat_name, c_wl_surface)
    }

    // TODO
    fn configure_popup(
        &mut self,
        _popup: &sctk::shell::xdg::popup::Popup,
        _config: sctk::shell::xdg::popup::PopupConfigure,
    ) {
    }

    fn close_popup(&mut self, popup: &sctk::shell::xdg::popup::Popup) {
        self.popups.retain(|p| {
            if p.c_popup.wl_surface() == popup.wl_surface() {
                if p.s_surface.alive() {
                    p.s_surface.send_popup_done();
                }
                false
            } else {
                true
            }
        });
    }

    // handled by custom method with access to renderer instead
    fn configure_layer(&mut self, _: &LayerSurface, _: LayerSurfaceConfigure) {}

    // handled by the container
    fn close_layer(&mut self, _: &LayerSurface) {}

    // handled by the container
    fn output_leave(
        &mut self,
        _c_output: c_wl_output::WlOutput,
        _s_output: Output,
    ) -> anyhow::Result<()> {
        anyhow::bail!("Output leaving should be handled by the container")
    }

    fn update_output(
        &mut self,
        c_output: c_wl_output::WlOutput,
        s_output: Output,
        info: OutputInfo,
    ) -> anyhow::Result<bool> {
        self.output.replace((c_output, s_output, info));
        self.dimensions = self.constrain_dim(self.dimensions.clone());
        self.is_dirty = true;
        Ok(true)
    }

    fn new_output<W: WrapperSpace>(
        &mut self,
        compositor_state: &sctk::compositor::CompositorState,
        fractional_scale_manager: Option<&FractionalScalingManager<W>>,
        viewport: Option<&ViewporterState<W>>,
        layer_state: &mut LayerShell,
        _conn: &sctk::reexports::client::Connection,
        qh: &QueueHandle<GlobalState<W>>,
        c_output: Option<c_wl_output::WlOutput>,
        s_output: Option<Output>,
        output_info: Option<OutputInfo>,
    ) -> anyhow::Result<()> {
        if self.output.is_some() {
            bail!("output already setup for this panel");
        }
        if let (Some(_c_output), Some(s_output), Some(output_info)) =
            (c_output.as_ref(), s_output.as_ref(), output_info.as_ref())
        {
            self.space.map_output(s_output, output_info.location);
            match &self.config.output {
                CosmicPanelOuput::Active => {
                    bail!("output does not match config")
                }
                CosmicPanelOuput::Name(config_name)
                    if output_info.name != Some(config_name.to_string()) =>
                {
                    bail!("output does not match config")
                }
                _ => {}
            };
            if matches!(self.config.output, CosmicPanelOuput::Active) && self.layer.is_some() {
                return Ok(());
            }
        } else if !matches!(self.config.output, CosmicPanelOuput::Active) {
            bail!("output does not match config");
        }
        let dimensions: Size<i32, Logical> = self.constrain_dim((0, 0).into());

        let layer = match self.config().layer() {
            zwlr_layer_shell_v1::Layer::Background => Layer::Background,
            zwlr_layer_shell_v1::Layer::Bottom => Layer::Bottom,
            zwlr_layer_shell_v1::Layer::Top => Layer::Top,
            zwlr_layer_shell_v1::Layer::Overlay => Layer::Overlay,
            _ => bail!("Invalid layer"),
        };

        let surface = compositor_state.create_surface(&qh);
        let client_surface =
            layer_state.create_layer_surface(&qh, surface, layer, Some("Panel"), c_output.as_ref());
        // client_surface.set_margin(margin.top, margin.right, margin.bottom, margin.left);
        client_surface.set_keyboard_interactivity(match self.config.keyboard_interactivity {
            xdg_shell_wrapper_config::KeyboardInteractivity::None => KeyboardInteractivity::None,
            xdg_shell_wrapper_config::KeyboardInteractivity::Exclusive => {
                KeyboardInteractivity::Exclusive
            }
            xdg_shell_wrapper_config::KeyboardInteractivity::OnDemand => {
                KeyboardInteractivity::OnDemand
            }
        });
        client_surface.set_size(
            dimensions.w.try_into().unwrap(),
            dimensions.h.try_into().unwrap(),
        );

        client_surface.set_anchor(match self.config.anchor {
            cosmic_panel_config::PanelAnchor::Left => Anchor::all().difference(Anchor::RIGHT),
            cosmic_panel_config::PanelAnchor::Right => Anchor::all().difference(Anchor::LEFT),
            cosmic_panel_config::PanelAnchor::Top => Anchor::all().difference(Anchor::BOTTOM),
            cosmic_panel_config::PanelAnchor::Bottom => Anchor::all().difference(Anchor::TOP),
        });

        if !self.config.expand_to_edges() {
            let input_region = Region::new(compositor_state)?;
            client_surface
                .wl_surface()
                .set_input_region(Some(input_region.wl_region()));
            self.input_region.replace(input_region);
        }

        let fractional_scale = fractional_scale_manager
            .map(|f| f.fractional_scaling(client_surface.wl_surface(), &qh));

        let viewport = viewport.map(|v| v.get_viewport(client_surface.wl_surface(), &qh));

        client_surface.commit();

        let next_render_event = Rc::new(Cell::new(Some(SpaceEvent::WaitConfigure {
            first: true,
            width: dimensions.w,
            height: dimensions.h,
        })));

        self.output = izip!(
            c_output.into_iter(),
            s_output.into_iter(),
            output_info.as_ref().cloned()
        )
        .next();
        self.layer = Some(client_surface);
        self.layer_fractional_scale = fractional_scale;
        self.layer_viewport = viewport;
        self.dimensions = dimensions;
        self.space_event = next_render_event;
        self.is_dirty = true;
        if let Err(err) = self.spawn_clients(
            self.s_display.clone().unwrap(),
            &qh,
            self.security_context_manager.clone(),
        ) {
            error!(?err, "Failed to spawn clients");
        }
        Ok(())
    }

    fn handle_events<W: WrapperSpace>(
        &mut self,
        _dh: &DisplayHandle,
        _qh: &QueueHandle<GlobalState<W>>,
        _popup_manager: &mut PopupManager,
        _time: u32,
    ) -> Instant {
        unimplemented!()
    }

    fn frame(&mut self, surface: &c_wl_surface::WlSurface, _time: u32) {
        if Some(surface) == self.layer.as_ref().map(|l| l.wl_surface()) {
            self.has_frame = true;
        } else if let Some(p) = self
            .popups
            .iter_mut()
            .find(|p| surface == p.c_popup.wl_surface())
        {
            p.has_frame = true;
        }
    }

    fn get_scale_factor(&self, surface: &s_WlSurface) -> std::option::Option<f64> {
        let client = surface.client();
        if self
            .clients_center
            .lock()
            .unwrap()
            .iter()
            .chain(self.clients_left.lock().unwrap().iter())
            .chain(self.clients_right.lock().unwrap().iter())
            .any(|c| Some(&c.1) == client.as_ref())
        {
            Some(self.scale)
        } else {
            None
        }
    }

    fn scale_factor_changed(
        &mut self,
        surface: &c_wl_surface::WlSurface,
        scale: f64,
        legacy: bool,
    ) {
        info!(
            "Scale factor changed {scale} for as surface in space \"{}\" on {}",
            self.config.name,
            self.output
                .as_ref()
                .and_then(|o| o.2.name.clone())
                .unwrap_or_else(|| "None".to_string())
        );
        if Some(surface) == self.layer.as_ref().map(|l| l.wl_surface()) {
            self.scale = scale;
            self.is_dirty = true;
            if legacy && self.layer_fractional_scale.is_none() {
                surface.set_buffer_scale(scale as i32);
            } else {
                surface.set_buffer_scale(1);
                if let Some(viewport) = self.layer_viewport.as_ref() {
                    viewport.set_destination(self.actual_size.w.max(1), self.actual_size.h.max(1));
                }

                for surface in self.space.elements().filter_map(|e| e.wl_surface().clone()) {
                    with_states(&surface, |states| {
                        with_fractional_scale(states, |fractional_scale| {
                            fractional_scale.set_preferred_scale(scale);
                        });
                    });
                }
            }
        }
        for popup in &mut self.popups {
            if popup.c_popup.wl_surface() != surface {
                continue;
            }
            popup.scale = scale;
            let Rectangle { loc, size } = popup.rectangle;
            if popup.state.is_none() {
                popup.state = Some(WrapperPopupState::Rectangle {
                    x: loc.x,
                    y: loc.y,
                    width: size.w,
                    height: size.h,
                });
            }

            with_states(&popup.s_surface.wl_surface(), |states| {
                with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(scale);
                });
            });
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _surface: &c_wl_surface::WlSurface,
        _new_transform: cctk::sctk::reexports::client::protocol::wl_output::Transform,
    ) {
        // TODO handle the preferred transform
    }
}
