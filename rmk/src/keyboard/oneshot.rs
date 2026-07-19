use embassy_futures::select::{Either, select};
use embassy_time::Timer;
use rmk_types::action::Action;
use rmk_types::modifier::ModifierCombination;

use crate::event::KeyboardEvent;
use crate::keyboard::Keyboard;

/// State machine for one shot keys
#[derive(Default)]
pub enum OneShotState<T> {
    /// First one shot key press
    Initial(T),
    /// One shot key was released before any other key, normal one shot behavior
    Single(T),
    /// Another key was pressed before one shot key was released, treat as a normal modifier/layer
    Held(T),
    /// One shot inactive
    #[default]
    None,
}

/// Outcome of updating the one-shot modifier state for a processed action.
pub(crate) enum OsmUpdate {
    /// One-shot modifier state unchanged.
    None,
    /// One-shot modifier was consumed by a normal key (quick-release on press or
    /// chain-mode on release); the key's own HID report already reflects it.
    Consumed,
    /// A layer key (`cancel_ossm_on_layer_enter`) cancelled an actively-held
    /// one-shot modifier; the caller must emit a release report to drop the
    /// modifier from the host.
    Cancelled,
}

/// Whether an action enters/modifies a layer (MO, TG, OSL, `DefaultLayer`, ...).
/// Such actions cancel an active one-shot modifier when
/// `behavior.one_shot_modifiers.cancel_ossm_on_layer_enter` is set.
pub(crate) fn action_is_layer_action(action: &Action) -> bool {
    matches!(
        action,
        Action::LayerOn(_)
            | Action::LayerOnWithModifier(_, _)
            | Action::LayerOff(_)
            | Action::LayerToggle(_)
            | Action::LayerToggleOnly(_)
            | Action::DefaultLayer(_)
            | Action::PersistentDefaultLayer(_)
            | Action::TriLayerLower
            | Action::TriLayerUpper
            | Action::OneShotLayer(_)
    )
}

impl<T> OneShotState<T> {
    /// Get the current one shot value if any
    pub fn value(&self) -> Option<&T> {
        match self {
            OneShotState::Initial(v) | OneShotState::Single(v) | OneShotState::Held(v) => Some(v),
            OneShotState::None => None,
        }
    }
}

impl<'a> Keyboard<'a> {
    pub(crate) async fn process_action_osm(&mut self, new_modifiers: ModifierCombination, event: KeyboardEvent) {
        let activate_on_keypress = self.keymap.one_shot_modifiers_config().activate_on_keypress;

        // Update one shot state
        if event.pressed {
            let mut was_active = false;
            // Add new modifier combination to existing one shot or init if none
            self.osm_state = match self.osm_state {
                OneShotState::None => OneShotState::Initial(new_modifiers),
                OneShotState::Initial(cur_modifiers) => OneShotState::Initial(cur_modifiers | new_modifiers),
                OneShotState::Single(cur_modifiers) => {
                    was_active = cur_modifiers & new_modifiers == new_modifiers;

                    if was_active {
                        let result = cur_modifiers & !new_modifiers;
                        // Remove the matching event from unprocessed_events queue
                        self.unprocessed_events.retain(|e| e.pos != event.pos);
                        // Send report for current osm_state modifiers
                        self.send_keyboard_report_with_resolved_modifiers(true).await;

                        if result.into_bits() == 0 {
                            OneShotState::None
                        } else {
                            OneShotState::Single(result)
                        }
                    } else {
                        OneShotState::Single(cur_modifiers | new_modifiers)
                    }
                }
                OneShotState::Held(cur_modifiers) => OneShotState::Held(cur_modifiers | new_modifiers),
            };

            self.update_osl(event);

            // Send report for updated osm_state modifiers
            if was_active || activate_on_keypress {
                self.send_keyboard_report_with_resolved_modifiers(true).await;
            } else {
                // OSM armed but not yet in HID (`activate_on_keypress = false`);
                // still notify displays so chips reflect the pending one-shot.
                self.publish_modifier_ui_state();
            }
        } else {
            match self.osm_state {
                OneShotState::Initial(cur_modifiers) | OneShotState::Single(cur_modifiers) => {
                    self.osm_state = OneShotState::Single(cur_modifiers);
                    // Released OSM key while still in one-shot window — UI should
                    // keep showing the armed modifier until timeout or consume.
                    self.publish_modifier_ui_state();
                    let timeout = Timer::after(self.keymap.one_shot_timeout());
                    match select(timeout, self.keyboard_event_subscriber.next_message_pure()).await {
                        Either::First(_) => {
                            // Timeout, release modifiers
                            self.update_osl(event);
                            self.osm_state = OneShotState::None;

                            // Send release report because modifiers were held
                            if activate_on_keypress {
                                self.send_keyboard_report_with_resolved_modifiers(false).await;
                            } else {
                                self.publish_modifier_ui_state();
                            }
                        }
                        Either::Second(e) => {
                            // New event, send it to queue
                            if self.unprocessed_events.push(e).is_err() {
                                warn!("Unprocessed event queue is full, dropping event");
                            }
                        }
                    }
                }
                OneShotState::Held(cur_modifiers) => {
                    let was_active = cur_modifiers & new_modifiers == new_modifiers;

                    if !was_active {
                        return;
                    }

                    // Release modifier
                    self.update_osl(event);
                    self.osm_state = OneShotState::None;

                    // This sends a separate hid report with the
                    // currently registered modifiers except the
                    // one shot modifiers -> this way "releasing" them.
                    self.send_keyboard_report_with_resolved_modifiers(false).await;
                }
                _ => (),
            };
        }
    }

    pub(crate) async fn process_action_osl(&mut self, layer_num: u8, event: KeyboardEvent) {
        // Update one shot state
        if event.pressed {
            // Deactivate old layer if any
            if let Some(&l) = self.osl_state.value() {
                self.keymap.deactivate_layer(l);
            }

            // Update layer of one shot
            self.osl_state = match self.osl_state {
                OneShotState::None => OneShotState::Initial(layer_num),
                OneShotState::Initial(_) => OneShotState::Initial(layer_num),
                OneShotState::Single(_) => OneShotState::Single(layer_num),
                OneShotState::Held(_) => OneShotState::Held(layer_num),
            };

            // Activate new layer
            self.keymap.activate_layer(layer_num);
        } else {
            match self.osl_state {
                OneShotState::Initial(l) | OneShotState::Single(l) => {
                    self.osl_state = OneShotState::Single(l);

                    let timeout = embassy_time::Timer::after(self.keymap.one_shot_timeout());
                    match select(timeout, self.keyboard_event_subscriber.next_message_pure()).await {
                        Either::First(_) => {
                            // Timeout, deactivate layer
                            self.keymap.deactivate_layer(layer_num);
                            self.osl_state = OneShotState::None;
                        }
                        Either::Second(e) => {
                            // New event, send it to queue
                            if self.unprocessed_events.push(e).is_err() {
                                warn!("Unprocessed event queue is full, dropping event");
                            }
                        }
                    }
                }
                OneShotState::Held(layer_num) => {
                    self.osl_state = OneShotState::None;
                    self.keymap.deactivate_layer(layer_num);
                }
                _ => (),
            };
        }
    }

    /// Update OSM state based on the keyboard event.
    /// Returns how the OSM was affected (see [`OsmUpdate`]).
    pub(crate) fn update_osm(&mut self, event: KeyboardEvent, action: Action) -> OsmUpdate {
        let config = self.keymap.one_shot_modifiers_config();
        let quick_release = config.quick_release;

        // With `cancel_ossm_on_layer_enter`, activating a layer key while a one-shot
        // modifier is active cancels that modifier. Cancellation happens on the layer
        // key *press* — the moment the layer actually becomes active — so the layer's
        // keys are used without the modifier. If no modifier is active, there is
        // nothing to cancel.
        if config.cancel_ossm_on_layer_enter && event.pressed && action_is_layer_action(&action) {
            if self.osm_state.value().is_some() {
                self.osm_state = OneShotState::None;
                return OsmUpdate::Cancelled;
            }
            return OsmUpdate::None;
        }

        match self.osm_state {
            OneShotState::Initial(m) if event.pressed => {
                // Another key was pressed while the one-shot modifier key is still
                // held: treat it as a normal held modifier from now on. A *release*
                // of another key must not trigger this transition (otherwise a
                // layer/one-shot release key being let go before the modifier key
                // would wrongly convert the modifier to `Held` and release it).
                self.osm_state = OneShotState::Held(m);
                OsmUpdate::None
            }
            OneShotState::Single(_) if quick_release && event.pressed => {
                self.osm_state = OneShotState::None;
                OsmUpdate::Consumed
            }
            OneShotState::Single(_)
                if !quick_release
                    && !event.pressed
                    && (!config.cancel_ossm_on_layer_enter || !action_is_layer_action(&action)) =>
            {
                // Chain mode: a *normal* key release consumes the one-shot. A layer
                // key release only does so when `cancel_ossm_on_layer_enter` is unset;
                // when it is set, cancellation is handled on the layer key's press
                // above, and its release must leave the modifier untouched.
                self.osm_state = OneShotState::None;
                OsmUpdate::Consumed
            }
            _ => OsmUpdate::None,
        }
    }

    pub(crate) fn update_osl(&mut self, event: KeyboardEvent) {
        match self.osl_state {
            OneShotState::Initial(l) => self.osl_state = OneShotState::Held(l),
            OneShotState::Single(layer_num) if !event.pressed => {
                self.keymap.deactivate_layer(layer_num);
                self.osl_state = OneShotState::None;
            }
            _ => (),
        }
    }
}
