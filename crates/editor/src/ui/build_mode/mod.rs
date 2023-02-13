use std::{sync::Arc, time::Duration};

use derive_more::Display;
use futures_signals::signal::SignalExt;
use itertools::Itertools;
use kiwi_core::{asset_cache, async_ecs::async_run, get_mouse_clip_space_position, runtime};
use kiwi_ecs::{uid_lookup, Component, ComponentValue, EntityUid, World};
use kiwi_element::{Element, ElementComponent, ElementComponentExt, Group, Hooks};
use kiwi_input::{on_app_keyboard_input, MouseButton};
use kiwi_intent::{client_push_intent, rpc_undo_head_exact};
use kiwi_network::{client::GameClient, log_network_error};
use kiwi_object::{rpc_load_object, MultiEntityUID};
use kiwi_std::{
    asset_url::{select_asset, AssetType},
    cb, cb_arc, friendly_id, Cb,
};
use kiwi_ui::{
    command_modifier,
    layout::{docking, width, Docking},
    margin, padding, space_between_items, use_interval_deps, Borders, Button, ButtonStyle, Dock, FlowRow, Hotkey, ScreenContainer,
    Separator, StylesExt, STREET,
};
use tokio::time::sleep;
use winit::event::{ElementState, VirtualKeyCode};

use super::{terrain_mode::GenerateTerrainButton, EditorPlayerInputHandler, EditorPrefs};
use crate::{
    intents::{intent_delete, intent_duplicate, intent_spawn_object2, IntentDuplicate, IntentSpawnObject2, SelectMode},
    ui::use_player_selection,
    Selection, GRID_SIZE,
};

mod entity_browser;
mod grid_material;
mod guide;
mod select_area;
mod selection_panel;
mod transform;

use guide::*;
use select_area::*;
use selection_panel::*;
use transform::*;

use self::entity_browser::EntityBrowserScreen;

/// An editor can only be in one action at a time.
/// They can be confirmed or aborted.
///
/// **Note**: Storing an EditorAction in the ecs may cause events to not undo for a time after the
/// entity is removed.
///
/// This is due to the builtin drop/removed events queue keeping the value alive
pub struct EditorAction<T: ComponentValue> {
    id: Option<String>,
    client: GameClient,
    runtime: tokio::runtime::Handle,
    tx: futures_signals::signal::Sender<Option<(String, T)>>,
    intent: Component<T>,
}

impl<T: ComponentValue> std::fmt::Debug for EditorAction<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorAction").field("id", &self.id).field("intent", &self.intent).finish()
    }
}

impl<T: ComponentValue> EditorAction<T> {
    pub fn new(runtime: tokio::runtime::Handle, client: GameClient, intent: Component<T>, throttle: Duration) -> Self {
        let (tx, rx) = futures_signals::signal::channel(None);

        {
            let client = client.clone();
            runtime.spawn({
                rx.throttle(move || sleep(throttle)).for_each(move |value| {
                    let client = client.clone();
                    async move {
                        if let Some((id, arg)) = value {
                            client_push_intent(client.clone(), intent, arg, Some(id), None).await;
                        }
                    }
                })
            });
        }

        Self { client, id: None, runtime, intent, tx }
    }

    #[tracing::instrument(skip_all, level = "info")]
    pub fn push_intent(&mut self, arg: T) {
        let id = self.id.get_or_insert_with(friendly_id).clone();
        let _ = self.tx.send(Some((id, arg)));
    }

    #[tracing::instrument(level = "info")]
    pub fn confirm(&mut self) {
        self.id = None
    }

    #[tracing::instrument(level = "info")]
    pub fn cancel(&self) {
        let id = self.id.clone();
        if let Some(id) = id {
            tracing::info!("Cancelling action: {id}");
            let client = self.client.clone();
            self.runtime.spawn(async move {
                client.rpc(rpc_undo_head_exact, id).await.unwrap();
            });
        }
    }
}

impl<T: ComponentValue> Drop for EditorAction<T> {
    fn drop(&mut self) {
        tracing::info!("Dropping editor action");
        self.cancel()
    }
}

#[derive(Debug, Clone)]
pub struct EditorBuildMode;
impl ElementComponent for EditorBuildMode {
    fn render(self: Box<Self>, world: &mut World, hooks: &mut Hooks) -> Element {
        let (game_client, _) = hooks.consume_context::<GameClient>().unwrap();
        let (selection, set_selection) = use_player_selection(world, hooks);
        // tracing::info!("Drawing EditorBuildMode: {selection:?}");

        let set_select_mode = hooks.provide_context(|| SelectMode::Set);
        let set_srt_mode = hooks.provide_context(|| None as Option<TransformMode>);
        let (screen, set_screen) = hooks.use_state(None);

        let targets = hooks.use_ref_with::<Arc<[EntityUid]>>(|| Arc::from([]));
        let rerender = hooks.use_rerender_signal();

        {
            let game_state = game_client.game_state.clone();
            let targets = targets.clone();
            let mut prev = None;

            let update_targets = move |selection: &Selection| {
                profiling::scope!("update_targets");
                let state = game_state.lock();

                let res = selection
                    .iter()
                    .filter_map(|uid| -> Option<_> {
                        match state.world.resource(uid_lookup()).get(&uid) {
                            Ok(id) if state.world.exists(id) => Some(uid),
                            _ => {
                                tracing::warn!("Entity: {uid} could not be resolved");
                                None
                            }
                        }
                    })
                    .collect_vec();

                if Some(&res) != prev.as_ref() {
                    tracing::info!("Resolving targets: {selection:?} => {res:?}");
                    prev = Some(res.clone());
                    *targets.lock() = res.into();
                    rerender();
                }
            };

            use_interval_deps(world, hooks, Duration::from_millis(2000), true, selection.clone(), update_targets);
        }

        // Make sure to get the value *after* the `use_interval_deps`
        let targets = targets.lock();

        Dock(vec![
            EditorPlayerInputHandler.el(),
            ScreenContainer(screen).el(),
            Group(vec![Element::new().listener(
                on_app_keyboard_input(),
                Arc::new(move |_, _, event| match event.keycode {
                    Some(VirtualKeyCode::LShift) => {
                        if event.state == ElementState::Pressed {
                            set_select_mode(SelectMode::Add);
                        } else {
                            set_select_mode(SelectMode::Set);
                        }
                        // Do not interfere with the player movement
                        false
                    }
                    Some(VirtualKeyCode::LControl) => {
                        if event.state == ElementState::Pressed {
                            set_select_mode(SelectMode::Remove);
                        } else {
                            set_select_mode(SelectMode::Set);
                        }
                        false
                    }
                    _ => false,
                }),
            )])
                .el(),
            if !selection.is_empty() {
                SelectionPanel { selection: selection.clone(), set_selection: set_selection.clone() }
                    .el()
                    .set(width(), 300.)
                    .set(docking(), Docking::Right)
                    .floating_panel()
                    .set(margin(), Borders::even(STREET))
                    .set(padding(), Borders::even(STREET))
            } else {
                Element::new()
            },
            FlowRow({
                let mut items = vec![
                    Button::new("\u{f405}", {
                        let set_srt_mode = set_srt_mode.clone();
                        let game_client = game_client.clone();
                        move |world| {
                            let set_srt_mode = set_srt_mode.clone();
                            let game_client = game_client.clone();
                            let async_run = world.resource(async_run()).clone();
                            select_asset(world.resource(asset_cache()), AssetType::Object, move |object_url| {
                                tracing::info!("got object_url: {object_url:?}");
                                if let Some(object_url) = object_url.random().cloned() {
                                    async_run.run(move |world| {
                                        let set_srt_mode = set_srt_mode.clone();
                                        let ray = {
                                            game_client.game_state.lock().screen_ray(get_mouse_clip_space_position(world))
                                        };
                                        let position = ray.origin + ray.dir * 10.;
                                        world.resource(runtime()).spawn(async move {
                                            if let Err(err) = game_client.rpc(rpc_load_object, object_url.clone()).await {
                                                log_network_error(&err.into());
                                                return;
                                            }
                                            client_push_intent(game_client, intent_spawn_object2(), IntentSpawnObject2 {
                                                object_url,
                                                entity_uid: MultiEntityUID::new(),
                                                position,
                                                select: true
                                            }, None, Some(Box::new(move || {
                                                set_srt_mode(Some(TransformMode::Place));
                                            }))).await;
                                        });
                                    });
                                }
                            });
                        }
                    })
                    .tooltip("Browse objects")
                    .style(ButtonStyle::Primary)
                    .hotkey(VirtualKeyCode::Tab)
                    .el(),
                    Separator { vertical: true }.el(),
                    Button::new("\u{f03a}", {
                        let set_selection = set_selection.clone();
                        let set_screen = set_screen.clone();
                        move |_| {
                            let set_selection = set_selection.clone();
                            set_screen(Some(
                                EntityBrowserScreen {
                                    on_select: cb_arc(Arc::new({
                                        let set_screen = set_screen.clone();
                                        move |_id, uid| {
                                            set_selection(Selection::new([uid]));
                                            set_screen(None);
                                        }
                                    })),
                                    on_back: cb({
                                        let set_screen = set_screen.clone();
                                        move || set_screen(None)
                                    }),
                                }
                                .el(),
                            ));
                        }
                    })
                    .tooltip("Browse entities")
                    .el(),
                ];
                if !selection.is_empty() {
                    items.extend([
                        Separator { vertical: true }.el(),
                        Button::new(
                            "\u{f68e}",
                            closure!(clone game_client, clone targets, clone set_srt_mode, |world| {
                                let set_srt_mode = set_srt_mode.clone();
                                let game_client = game_client.clone();

                                tracing::info!("Duplicating {targets:?}");
                                world.resource(runtime()).spawn(
                                    client_push_intent(game_client, intent_duplicate(), IntentDuplicate { new_uids: targets.iter().map(|_| EntityUid::create()).collect(), entities: targets.to_vec(), select: true }, None, Some(Box::new(move || {
                                        tracing::info!("Entering translate move");


                                        set_srt_mode(Some(TransformMode::Translate));
                                    })))
                                );
                            }),
                        )
                            .tooltip("Duplicate")
                            .hotkey(VirtualKeyCode::D)
                            .hotkey_modifier(command_modifier())
                            .el(),
                        Button::new("\u{f6bf}", {
                            let targets = targets.clone();
                            move |world| {
                                world.resource(runtime()).spawn(client_push_intent(
                                    game_client.clone(),
                                    intent_delete(),
                                    targets.to_vec(),
                                    None,
                                    None,
                                ));
                            }
                        })
                            .tooltip("Delete")
                            .hotkey(VirtualKeyCode::Back)
                            .el(),
                        Separator { vertical: true }.el(),
                        TransformControls { targets: targets.clone() }.el().key(format!("{selection:?}")),
                    ])
                }
                items
            })
                .el()
                .floating_panel()
                .set(docking(), Docking::Top)
                .set(space_between_items(), STREET)
                .set(margin(), Borders::even(STREET))
                .set(padding(), Borders::even(STREET)),
            GenerateTerrainButton.el()
                .set(margin(), Borders::even(STREET)),
            SelectArea.el(),
        ])
            .el()
    }
}

#[derive(Display, Debug, Clone, Copy, PartialEq, Eq)]
enum TransformMode {
    #[display(fmt = "Translate")]
    Translate,
    #[display(fmt = "Rotate")]
    Rotate,
    #[display(fmt = "Scale")]
    Scale,
    #[display(fmt = "Place")]
    Place,
}

#[derive(Debug, Clone)]
pub struct TransformControls {
    targets: Arc<[EntityUid]>,
}
impl ElementComponent for TransformControls {
    fn render(self: Box<Self>, _: &mut World, hooks: &mut Hooks) -> Element {
        let Self { targets } = *self;

        let (srt_mode, set_srt_mode) = hooks.consume_context::<Option<TransformMode>>().unwrap();

        let (prefs, set_prefs) = hooks.consume_context::<EditorPrefs>().unwrap();
        let set = set_prefs.clone();
        let set_snap_mode = move |snap| (set)(EditorPrefs { snap, ..prefs });
        let set_global_coordinates = move |use_global| (set_prefs)(EditorPrefs { use_global_coordinates: use_global, ..prefs });

        let mode_button = |mode, icon, hotkey| {
            Button::new(
                icon, // \u{f047}",
                closure!(clone set_srt_mode, |_| {
                    set_srt_mode(Some(mode));
                }),
            )
            .tooltip(mode.to_string())
            .hotkey(hotkey)
            .toggled(srt_mode == Some(mode))
        };

        let mut items = vec![
            Button::new(
                "",
                closure!(clone set_snap_mode, |_| {
                    if prefs.snap.is_some() {
                        set_snap_mode(None)
                    } else {
                        set_snap_mode(Some(GRID_SIZE))
                    }
                }),
            )
            .tooltip("Snap to grid")
            .hotkey(VirtualKeyCode::H)
            .toggled(prefs.snap.is_some())
            .el(),
            // TODO: Dropdown for `local/global`
            Button::new("", move |_| {
                set_global_coordinates(!prefs.use_global_coordinates);
            })
            .tooltip("Align to world space")
            .hotkey(VirtualKeyCode::U)
            .toggled(prefs.use_global_coordinates)
            .el(),
            Separator { vertical: true }.el(),
            mode_button(TransformMode::Translate, "", VirtualKeyCode::Key1).el(),
            mode_button(TransformMode::Rotate, "北", VirtualKeyCode::Key2).el(),
            mode_button(TransformMode::Scale, "ﬕ", VirtualKeyCode::Key3).el(),
            mode_button(TransformMode::Place, "", VirtualKeyCode::Key4).el(),
        ];

        let on_click: Cb<dyn Fn(MouseButton) + Sync + Send> = cb({
            let set_srt_mode = set_srt_mode.clone();
            move |_| {
                set_srt_mode(None);
            }
        });

        if srt_mode.is_some() {
            items.extend(vec![
                match (targets.is_empty(), srt_mode) {
                    (false, Some(TransformMode::Translate)) => TranslationController { targets, on_click }.el(),
                    (false, Some(TransformMode::Scale)) => ScaleController { targets, on_click }.el(),
                    (false, Some(TransformMode::Rotate)) => RotateController { targets, on_click }.el(),
                    (false, Some(TransformMode::Place)) => PlaceController { targets, on_click }.el(),
                    _ => Element::new(),
                },
                Hotkey::new(
                    VirtualKeyCode::Escape,
                    move |_| {
                        set_srt_mode(None);
                        // The editors are responsible for undoing the intents
                    },
                    Element::new(),
                )
                .el(),
            ]);
        }
        FlowRow(items).el().set(space_between_items(), STREET)
    }
}