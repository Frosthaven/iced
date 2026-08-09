use crate::platform_specific::wayland::{
    event_loop::state::SctkState, sctk_event::SctkEvent,
};
use cctk::sctk::{
    delegate_pointer,
    reexports::client::{
        protocol::wl_pointer::{self, WlPointer},
        Connection, Dispatch, Proxy, QueueHandle,
    },
    seat::{
        pointer::{
            CursorIcon, PointerData, PointerEvent, PointerEventKind,
            PointerHandler,
        },
        SeatState,
    },
};

impl PointerHandler for SctkState {
    fn pointer_frame(
        &mut self,
        conn: &cctk::sctk::reexports::client::Connection,
        _qh: &cctk::sctk::reexports::client::QueueHandle<Self>,
        pointer: &cctk::sctk::reexports::client::protocol::wl_pointer::WlPointer,
        events: &[cctk::sctk::seat::pointer::PointerEvent],
    ) {
        let (is_active, my_seat) =
            match self.seats.iter_mut().enumerate().find_map(|(i, s)| {
                if s.ptr.as_ref().map(|p| p.pointer()) == Some(pointer) {
                    Some((i, s))
                } else {
                    None
                }
            }) {
                Some((i, s)) => (i == 0, s),
                None => return,
            };

        // track events, but only forward for the active seat
        for e in events {
            if my_seat.hidden {
                // A hidden pointer has to be re-hidden on every enter: the
                // compositor restores its own cursor image then, and the
                // enter serial the hide request needs only exists from that
                // point on.
                if matches!(e.kind, PointerEventKind::Enter { .. }) {
                    my_seat.hide_cursor();
                }
            } else if my_seat.active_icon != my_seat.icon {
                // Restore cursor that was set by appliction, or default
                my_seat.set_cursor(
                    conn,
                    my_seat.icon.unwrap_or(CursorIcon::Default),
                );
            }

            if is_active {
                let id = winit::window::WindowId::from_raw(
                    e.surface.id().as_ptr() as usize,
                );
                if self.windows.iter().any(|w| w.window.id() == id) {
                    continue;
                }

                self.sctk_events.push(SctkEvent::PointerEvent {
                    variant: PointerEvent {
                        surface: e.surface.clone(),
                        position: e.position,
                        kind: e.kind.clone(),
                    },
                    ptr_id: pointer.clone(),
                    seat_id: my_seat.seat.clone(),
                });
            }
            match e.kind {
                PointerEventKind::Enter { .. } => {
                    _ = my_seat.ptr_focus.replace(e.surface.clone());
                }
                PointerEventKind::Leave { .. } => {
                    _ = my_seat.ptr_focus.take();
                    _ = my_seat.active_icon = None;
                }
                PointerEventKind::Press {
                    time,
                    button,
                    serial,
                } => {
                    _ = my_seat.last_ptr_press.replace((time, button, serial));
                }
                // TODO revisit events that ought to be handled and change internal state
                _ => {}
            }
        }
    }
}

// Keep upstream's cursor-shape delegation exactly as `delegate_pointer!(SctkState)`
// writes it. Only the `wl_pointer` half below is ours.
delegate_pointer!(SctkState, pointer: []);

/// `wl_pointer` dispatch, wrapping smithay-client-toolkit's.
///
/// This exists ONLY to work around a compositor bug. cosmic-comp sends
/// `wl_pointer.enter` WITHOUT the `wl_pointer.frame` that must terminate it.
/// `frame` has been mandatory since `wl_seat` version 5, and the spec says one
/// is sent for every logical event group "even if the group only contains a
/// single wl_pointer event". We bind `wl_seat` at version 9.
///
/// sctk buffers pointer events at v5+ and drains the buffer only in its `Frame`
/// arm (`smithay-client-toolkit-0.20.0`, `src/seat/pointer/mod.rs`, the
/// `wl_pointer::Event::Frame` arm of `impl<D, U> Dispatch<WlPointer, U, D> for
/// SeatState`). That buffering is CORRECT. With no frame the enter simply sits
/// in the buffer, `PointerHandler::pointer_frame` is never called, and a client
/// on a layer surface learns neither that the pointer arrived nor where it is,
/// until some later event that DOES carry a frame flushes it. In practice that
/// is the first motion, which is the whole of "nothing happens until you move
/// the mouse".
///
/// We cannot fix it in sctk's own terms: `PointerData::inner` is `pub(crate)`,
/// so the buffered surface and position are unreachable from here, and
/// `PointerData` has no setters, so reimplementing this dispatch outright would
/// stop `latest_enter` and `latest_btn` ever being written. Those back
/// `ThemedPointer::set_cursor`, `ThemedPointer::hide_cursor` and popup grabs, so
/// that route trades one broken thing for three.
///
/// So we delegate to upstream unchanged and add exactly ONE departure: after an
/// `Enter`, hand upstream a synthetic `Frame` to flush the batch it is holding.
///
/// # Why the extra frame is harmless, in four cases
///
/// All four rest on ONE line of upstream, the `if !pending.is_empty()` guard in
/// its `Frame` arm. **Re-check that guard after any sctk bump**: if it ever goes
/// away, our synthetic frame starts dispatching empty batches and, worse, the
/// real frame that follows starts dispatching the enter a second time.
///
/// 1. **cosmic-comp** (enter, no frame): the buffer holds the enter, we flush,
///    `pointer_frame` fires with it. This is the bug being fixed.
/// 2. **A compliant compositor** (enter, then frame): we flush first, so the
///    real frame finds an empty buffer and the guard makes it a no-op. Exactly
///    one dispatch, not two.
/// 3. **A `Leave` and an `Enter` grouped in one frame** (the case sctk's own
///    `PointerHandler` docs call out): leave is pushed, enter is pushed, we
///    flush `[Leave, Enter]` in that order, and the real frame no-ops. Same
///    events, same order, one batch, exactly as if the frame had done it.
/// 4. **`wl_pointer` below version 5**: upstream dispatched the enter
///    immediately and never buffered, so the buffer is empty and our frame is a
///    no-op. Degrades safely rather than double-dispatching.
///
/// There is no deadlock: upstream's `event` releases the `PointerData` guard
/// before returning, and our two calls are sequential.
impl Dispatch<WlPointer, PointerData> for SctkState {
    fn event(
        state: &mut Self,
        proxy: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        data: &PointerData,
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        let was_enter = matches!(event, wl_pointer::Event::Enter { .. });

        <SeatState as Dispatch<WlPointer, PointerData, Self>>::event(
            state, proxy, event, data, conn, qhandle,
        );

        // THE DEPARTURE FROM UPSTREAM. Everything else on this path is sctk's.
        if was_enter {
            <SeatState as Dispatch<WlPointer, PointerData, Self>>::event(
                state,
                proxy,
                wl_pointer::Event::Frame,
                data,
                conn,
                qhandle,
            );
        }
    }

    fn event_created_child(
        opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn cctk::sctk::reexports::client::backend::ObjectData>
    {
        <SeatState as Dispatch<WlPointer, PointerData, Self>>::event_created_child(
            opcode, qhandle,
        )
    }
}
