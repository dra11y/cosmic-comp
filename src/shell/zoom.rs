use std::{sync::Mutex, time::Instant};

use calloop::LoopHandle;
use cosmic::{
    Apply,
    iced::{Alignment, Background, Border, Length, alignment::Vertical},
    iced_widget, theme,
    widget::{self, icon::Named},
};
use cosmic_comp_config::{ZoomConfig, ZoomMovement};
use cosmic_config::ConfigSet;
use keyframe::{ease, functions::Linear};
use smithay::{
    backend::renderer::{ImportMem, Renderer, element::AsRenderElements},
    desktop::space::SpaceElement,
    input::{
        Seat,
        pointer::{
            AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
            MotionEvent as PointerMotionEvent, PointerTarget, RelativeMotionEvent,
        },
        touch::{
            DownEvent, FrameMarker, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent,
            TouchTarget, UpEvent,
        },
    },
    output::Output,
    utils::{FrameExtents, IsAlive, Point, Rectangle, Serial, Size},
};
use tracing::error;

use crate::{
    state::State,
    utils::{
        iced::{IcedElement, Program},
        prelude::*,
        tween::EasePoint,
    },
};

use super::{
    ANIMATION_DURATION, check_grab_preconditions,
    focus::target::PointerFocusTarget,
    grabs::{ContextMenu, Item, MenuAlignment, MenuGrab},
};

const MAX_ZOOM: f64 = 100.;

#[derive(Debug, Clone)]
pub struct ZoomState {
    pub(super) seat: Seat<State>,
    pub(super) show_overlay: bool,
}

#[derive(Debug)]
pub struct OutputZoomState {
    level: f64,
    movement: ZoomMovement,
    pointer_position: Point<f64, Local>,
    previous_level: Option<(f64, Instant)>,
    focal_point: Point<f64, Local>,
    previous_point: Option<(Point<f64, Local>, Instant)>,
    element: ZoomElement,
}

impl OutputZoomState {
    pub fn new(
        output: &Output,
        config: &ZoomConfig,
        level: f64,
        loop_handle: LoopHandle<'static, State>,
        theme: cosmic::Theme,
    ) -> OutputZoomState {
        let output_geometry = output.geometry().to_f64();
        let focal_point = Point::new(0., 0.);

        let program = ZoomProgram::new(level, config.view_moves, config.increment);
        let element = IcedElement::new(program, Size::default(), loop_handle, theme);
        let mut size = element.minimum_size();
        size.w = (size.w + 32/*TODO: figure out why iced is calculating too little*/)
            .min(output_geometry.size.w.round() as i32);
        element.set_activate(true);
        element.resize(size);
        element.output_enter(output, Rectangle::new(Point::from((0, 0)), size));
        element.set_additional_scale(level.min(4.));

        OutputZoomState {
            level,
            pointer_position: focal_point,
            previous_level: None,
            focal_point,
            previous_point: None,
            element,
            movement: config.view_moves,
        }
    }

    pub fn global_pos_to_screen_space(
        &self,
        pos: Point<f64, Global>,
        output: &Output,
    ) -> Point<f64, Local> {
        let zoomed_output_geometry = self.zoomed_geometry_global(output, None);
        let level = self.animating_level();

        // lets try to get the global cursor position into screen space
        let relative_to_zoom_geo = Point::<f64, Local>::from((
            pos.x - zoomed_output_geometry.loc.x,
            pos.y - zoomed_output_geometry.loc.y,
        ));
        relative_to_zoom_geo.upscale(level)
    }

    pub fn update_pointer_position(&mut self, output: &Output, position: Point<f64, Local>) {
        if position == self.pointer_position {
            return;
        }
        self.pointer_position = position;
        self.update_focal_point(output);
    }

    pub fn update_movement(&mut self, output: &Output, movement: ZoomMovement) {
        if movement == self.movement {
            return;
        }
        self.movement = movement;
        self.previous_point = Some((self.focal_point, Instant::now()));
        self.update_focal_point(output);
    }

    pub fn surface_under(
        &self,
        output: &Output,
        pos: Point<f64, Global>,
    ) -> Option<(PointerFocusTarget, Point<f64, Global>)> {
        let output_geometry = output.geometry().to_f64();
        let zoomed_output_geometry = self.zoomed_geometry_global(output, Some(output_geometry));
        let local_pos = self.global_pos_to_screen_space(pos, output);

        let size = self.element.current_size().to_f64().as_local();
        let location = Point::<f64, Local>::from((
            output_geometry.size.w / 2. - size.w / 2.,
            output_geometry.size.h / 4. * 3. - size.h / 2.,
        ));
        let area = Rectangle::<_, Local>::new(location, size);

        if area.contains(local_pos) {
            return Some((PointerFocusTarget::ZoomUI(self.element.clone().into()), {
                // and vise-versa from screen-space to zoom-space...
                let scaled_loc = location.downscale(self.level);
                let global_loc = Point::<f64, Global>::from((scaled_loc.x, scaled_loc.y))
                    + zoomed_output_geometry.loc;

                // HACK: We do have the right position now `global_loc`, but smithay calculates
                // the relative position for us... Which will be wrong given the cursor movement will
                // be scaled, while this element isn't, as it exists in screen-space and not workspace-space.
                // So we shift the location relatively to make up for the scaled movement...
                let diff = (pos - global_loc).upscale(self.level - 1.);

                global_loc - diff
            }));
        }

        None
    }

    pub fn zoomed_geometry(
        &self,
        output: &Output,
        output_geometry: Option<Rectangle<f64, Local>>,
    ) -> Rectangle<f64, Local> {
        let output_geometry =
            output_geometry.unwrap_or_else(|| output.geometry().to_f64().to_local(output));

        let mut zoomed_output_geo = output_geometry.to_f64();
        zoomed_output_geo.loc -= self.focal_point;
        zoomed_output_geo = zoomed_output_geo.downscale(self.level);
        zoomed_output_geo.loc += self.focal_point;

        zoomed_output_geo
    }

    pub fn zoomed_geometry_global(
        &self,
        output: &Output,
        output_geometry: Option<Rectangle<f64, Global>>,
    ) -> Rectangle<f64, Global> {
        let mut zoomed_output_geo = output_geometry.unwrap_or_else(|| output.geometry().to_f64());
        let focal_point_global = self.focal_point.to_global(output);
        zoomed_output_geo.loc -= focal_point_global;
        zoomed_output_geo = zoomed_output_geo.downscale(self.animating_level());
        zoomed_output_geo.loc += focal_point_global;
        zoomed_output_geo
    }

    fn update_focal_point(&mut self, output: &Output) {
        let movement = self.movement;

        let output_geometry = output.geometry().to_f64().to_local(output);
        let zoomed_output_geometry = self.zoomed_geometry(output, Some(output_geometry));

        let animating_level = self.animating_level();

        match movement {
            ZoomMovement::Continuously => self.focal_point = self.pointer_position,
            ZoomMovement::OnEdge => {
                // Compute small margin relative to zoomed output to keep cursor within
                let margin_size = zoomed_output_geometry.size.h * 0.02;
                let margins = FrameExtents::new(margin_size, margin_size, margin_size, margin_size);
                let inner_rect = zoomed_output_geometry - margins;

                if inner_rect.contains(self.pointer_position) {
                    // Do not move if cursor within margins
                    return;
                }

                // Compute dx and dy to move the zoomed output based on cursor distance outside margin(s)
                let dx = if self.pointer_position.x < inner_rect.loc.x {
                    self.pointer_position.x - inner_rect.loc.x
                } else if self.pointer_position.x > inner_rect.loc.x + inner_rect.size.w {
                    self.pointer_position.x - (inner_rect.loc.x + inner_rect.size.w)
                } else {
                    0.0
                };
                let dy = if self.pointer_position.y < inner_rect.loc.y {
                    self.pointer_position.y - inner_rect.loc.y
                } else if self.pointer_position.y > inner_rect.loc.y + inner_rect.size.h {
                    self.pointer_position.y - (inner_rect.loc.y + inner_rect.size.h)
                } else {
                    0.0
                };

                let mut focal_point = self.focal_point + Point::new(dx, dy);

                // Clamp to output
                focal_point.x = focal_point.x.clamp(
                    output_geometry.loc.x,
                    output_geometry.loc.x + output_geometry.size.w - 1.0,
                );
                focal_point.y = focal_point.y.clamp(
                    output_geometry.loc.y,
                    output_geometry.loc.y + output_geometry.size.h - 1.0,
                );

                self.focal_point = focal_point;
            }
            ZoomMovement::Centered => {
                let center = (output_geometry.size / 2.).to_point();

                // Compute translation to keep cursor at center of screen
                let mut tx = center.x - self.pointer_position.x * animating_level;
                let mut ty = center.y - self.pointer_position.y * animating_level;

                // Clamp translation to keep viewport within screen bounds
                tx = tx.clamp(output_geometry.size.w * (1.0 - animating_level), 0.0);
                ty = ty.clamp(output_geometry.size.h * (1.0 - animating_level), 0.0);

                // Convert translation back to focal point:  T = F * (1 - level)
                self.focal_point =
                    Point::from((tx / (1.0 - animating_level), ty / (1.0 - animating_level)));
            }
        }
    }

    pub fn animating_focal_point(&mut self, output: &Output) -> Point<f64, Local> {
        if self.is_animating() {
            // drive animation forward
            self.update_focal_point(output);
        }

        if let Some((old_point, start)) = self.previous_point.as_ref() {
            let duration_since = Instant::now().duration_since(*start);
            if duration_since > ANIMATION_DURATION {
                self.previous_point.take();
                return self.focal_point;
            }

            let percentage =
                duration_since.as_millis() as f32 / ANIMATION_DURATION.as_millis() as f32;
            ease(
                Linear,
                EasePoint(*old_point),
                EasePoint(self.focal_point),
                percentage,
            )
            .0
        } else {
            self.focal_point
        }
    }

    pub fn target_level(&self) -> f64 {
        self.level
    }

    pub fn animating_level(&self) -> f64 {
        if let Some((old_level, start)) = self.previous_level.as_ref() {
            let percentage = Instant::now().duration_since(*start).as_millis() as f32
                / ANIMATION_DURATION.as_millis() as f32;

            ease(Linear, *old_level, self.level, percentage)
        } else {
            self.level
        }
    }

    pub fn is_animating(&self) -> bool {
        self.previous_point.is_some() || self.previous_level.is_some()
    }

    pub fn refresh(&mut self) -> bool {
        if self
            .previous_level
            .as_ref()
            .is_some_and(|(_, start)| Instant::now().duration_since(*start) > ANIMATION_DURATION)
        {
            self.previous_level.take();
        }
        self.element.refresh();
        self.level == 1. && self.previous_level.is_none()
    }

    pub fn update_level(
        &mut self,
        output: &Output,
        pointer_position: Point<f64, Local>,
        level: f64,
        animate: bool,
        increment: u32,
    ) {
        self.pointer_position = pointer_position;
        if level == self.level {
            return;
        }
        if self.level == 1. {
            self.focal_point = pointer_position;
            self.update_focal_point(output);
        }
        if animate {
            let now = Instant::now();
            self.previous_level = Some((self.animating_level(), now));
            if self.movement == ZoomMovement::OnEdge {
                self.previous_point = Some((self.focal_point, now));
            }
        }
        self.level = level.clamp(1.0, MAX_ZOOM);
        self.update_focal_point(output);
        self.element.set_additional_scale(level.min(4.));
        self.element.queue_message(ZoomMessage::Update {
            level,
            movement: self.movement,
            increment,
        });
    }

    fn render<R, C>(&mut self, renderer: &mut R, output: &Output) -> Vec<C>
    where
        C: From<<IcedElement<ZoomProgram> as AsRenderElements<R>>::RenderElement>,
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        let size = self.element.current_size().to_f64();
        let output_geo = output.geometry().to_f64();
        let scale = output.current_scale();
        let location = Point::from((
            output_geo.size.w / 2. - size.w / 2.,
            output_geo.size.h / 4. * 3. - size.h / 2.,
        ))
        .to_physical(scale.fractional_scale())
        .to_i32_round();

        self.element
            .render_elements(renderer, location, scale.fractional_scale().into(), 1.0)
    }
}

impl ZoomState {
    pub fn current_seat(&self) -> Seat<State> {
        self.seat.clone()
    }

    pub fn surface_under(
        &self,
        output: &Output,
        pos: Point<f64, Global>,
    ) -> Option<(PointerFocusTarget, Point<f64, Global>)> {
        self.output_state(output)
            .lock()
            .unwrap()
            .surface_under(output, pos)
    }

    pub fn output_state<'a>(&self, output: &'a Output) -> &'a Mutex<OutputZoomState> {
        output.user_data().get::<Mutex<OutputZoomState>>().unwrap()
    }

    pub fn target_level(&self, output: &Output) -> f64 {
        self.output_state(output).lock().unwrap().target_level()
    }

    pub fn animating_focal_point_and_level(&self, output: &Output) -> (Point<f64, Local>, f64) {
        let mut output_state = self.output_state(output).lock().unwrap();
        (
            output_state.animating_focal_point(output),
            output_state.animating_level(),
        )
    }

    pub fn render<R, C>(renderer: &mut R, output: &Output) -> Vec<C>
    where
        C: From<<IcedElement<ZoomProgram> as AsRenderElements<R>>::RenderElement>,
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
        output_state.lock().unwrap().render(renderer, output)
    }
}

pub type ZoomElement = IcedElement<ZoomProgram>;

pub struct ZoomProgram {
    level: f64,
    increments: Vec<u32>,
    increment_idx: usize,
    movement: ZoomMovement,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoomMessage {
    Decrease,
    Increase,
    Increment,
    More,
    Close,
    Update {
        level: f64,
        increment: u32,
        movement: ZoomMovement,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuMessage {
    ViewContinuously,
    ViewOnEdge,
    ViewCentered,
    OpenSettings,
}

impl ZoomProgram {
    pub fn new(level: f64, movement: ZoomMovement, increment: u32) -> Self {
        let mut increments = vec![25, 50, 100, 150, 200];
        if !increments.contains(&increment) {
            increments.push(increment);
        }
        increments.sort();
        let increment_idx = increments.iter().position(|val| *val == increment).unwrap();

        ZoomProgram {
            level,
            increments,
            increment_idx,
            movement,
        }
    }
}

impl Program for ZoomProgram {
    type Message = ZoomMessage;

    fn view(&self) -> cosmic::Element<'_, Self::Message> {
        widget::row::with_children(vec![
            widget::button::icon(Named::new("list-remove-symbolic").size(16).prefer_svg(true))
                .on_press(ZoomMessage::Decrease)
                .into(),
            widget::text(format!("{}%", (self.level * 100.).round()))
                .align_y(Vertical::Center)
                .width(Length::Shrink)
                .into(),
            widget::button::icon(Named::new("list-add-symbolic").size(16).prefer_svg(true))
                .on_press(ZoomMessage::Increase)
                .into(),
            widget::divider::vertical::default().into(),
            widget::button::text(format!("{}%", self.increments[self.increment_idx]))
                .trailing_icon(Named::new("pan-down-symbolic").size(16).prefer_svg(true))
                .on_press(ZoomMessage::Increment)
                .class(theme::Button::MenuFolder)
                .into(),
            widget::button::icon(Named::new("view-more-symbolic").size(16).prefer_svg(true))
                .on_press(ZoomMessage::More)
                .into(),
            widget::divider::vertical::default().into(),
            widget::button::icon(
                Named::new("window-close-symbolic")
                    .size(16)
                    .prefer_svg(true),
            )
            .on_press(ZoomMessage::Close)
            .into(),
        ])
        .spacing(8.)
        .height(Length::Fixed(32.))
        .width(Length::Shrink)
        .align_y(Alignment::Center)
        .apply(widget::container)
        .padding(8)
        .class(theme::Container::custom(|theme| {
            let cosmic = theme.cosmic();
            let component = &cosmic.background.component;
            iced_widget::container::Style {
                snap: true,
                icon_color: Some(component.on.into()),
                text_color: Some(component.on.into()),
                background: Some(Background::Color(component.base.into())),
                border: Border {
                    radius: cosmic.radius_s().into(),
                    width: 1.0,
                    color: component.divider.into(),
                },
                shadow: Default::default(),
            }
        }))
        .into()
    }

    fn update(
        &mut self,
        message: Self::Message,
        loop_handle: &LoopHandle<'static, State>,
        last_seat: Option<&(Seat<State>, Serial)>,
    ) -> cosmic::Task<Self::Message> {
        match message {
            ZoomMessage::Decrease => {
                let _ = loop_handle.insert_idle(|state| {
                    let seat = state.common.shell.read().seats.last_active().clone();
                    let increment =
                        state.common.config.cosmic_conf.accessibility_zoom.increment as f64 / 100.0;

                    state.update_zoom(&seat, -increment, true);
                });
            }
            ZoomMessage::Increase => {
                let _ = loop_handle.insert_idle(|state| {
                    let seat = state.common.shell.read().seats.last_active().clone();
                    let increment =
                        state.common.config.cosmic_conf.accessibility_zoom.increment as f64 / 100.0;

                    state.update_zoom(&seat, increment, true);
                });
            }
            ZoomMessage::More => {
                let movement = self.movement;
                if let Some((seat, serial)) = last_seat.cloned() {
                    let _ = loop_handle.insert_idle(move |state| {
                        if let Some(start_data) =
                            check_grab_preconditions(&seat, Some(serial), None)
                        {
                            let shell = state.common.shell.read();
                            let output = seat.active_output();

                            if shell.zoom_state().is_some() {
                                let output_state =
                                    output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
                                let output_state_ref = output_state.lock().unwrap();
                                let location = output_state_ref.global_pos_to_screen_space(
                                    start_data.location().as_global(),
                                    &output,
                                );

                                let output_geometry = output.geometry();

                                let elem_size =
                                    output_state_ref.element.current_size().to_f64().as_local();
                                let elem_location = Point::<f64, Local>::from((
                                    output_geometry.size.w as f64 / 2. - elem_size.w / 2.,
                                    output_geometry.size.h as f64 / 4. * 3. - elem_size.h / 2.,
                                ));
                                let position = Point::<_, Local>::from((
                                    location.x,
                                    elem_location.y + elem_size.h / 2.,
                                ));
                                let level = output_state_ref.level;
                                std::mem::drop(output_state_ref);

                                let grab = MenuGrab::new(
                                    start_data,
                                    &seat,
                                    vec![
                                        Item::new(
                                            crate::fl!("a11y-zoom-move-continuously"),
                                            move |handle| {
                                                let _ = handle.insert_idle(move |state| {
                                                    state
                                                        .common
                                                        .config
                                                        .cosmic_conf
                                                        .accessibility_zoom
                                                        .view_moves = ZoomMovement::Continuously;
                                                    if let Err(err) =
                                                        state.common.config.cosmic_helper.set(
                                                            "accessibility_zoom",
                                                            state
                                                                .common
                                                                .config
                                                                .cosmic_conf
                                                                .accessibility_zoom,
                                                        )
                                                    {
                                                        error!(
                                                            ?err,
                                                            "Failed to update zoom config"
                                                        );
                                                    }
                                                    state.common.update_config();
                                                });
                                            },
                                        )
                                        .toggled(movement == ZoomMovement::Continuously),
                                        Item::new(
                                            crate::fl!("a11y-zoom-move-onedge"),
                                            move |handle| {
                                                let _ = handle.insert_idle(move |state| {
                                                    state
                                                        .common
                                                        .config
                                                        .cosmic_conf
                                                        .accessibility_zoom
                                                        .view_moves = ZoomMovement::OnEdge;
                                                    if let Err(err) =
                                                        state.common.config.cosmic_helper.set(
                                                            "accessibility_zoom",
                                                            state
                                                                .common
                                                                .config
                                                                .cosmic_conf
                                                                .accessibility_zoom,
                                                        )
                                                    {
                                                        error!(
                                                            ?err,
                                                            "Failed to update zoom config"
                                                        );
                                                    }
                                                    state.common.update_config();
                                                });
                                            },
                                        )
                                        .toggled(movement == ZoomMovement::OnEdge),
                                        Item::new(
                                            crate::fl!("a11y-zoom-move-centered"),
                                            move |handle| {
                                                let _ = handle.insert_idle(move |state| {
                                                    state
                                                        .common
                                                        .config
                                                        .cosmic_conf
                                                        .accessibility_zoom
                                                        .view_moves = ZoomMovement::Centered;
                                                    if let Err(err) =
                                                        state.common.config.cosmic_helper.set(
                                                            "accessibility_zoom",
                                                            state
                                                                .common
                                                                .config
                                                                .cosmic_conf
                                                                .accessibility_zoom,
                                                        )
                                                    {
                                                        error!(
                                                            ?err,
                                                            "Failed to update zoom config"
                                                        );
                                                    }
                                                    state.common.update_config();
                                                });
                                            },
                                        )
                                        .toggled(movement == ZoomMovement::Centered),
                                        Item::Separator,
                                        Item::new(crate::fl!("a11y-zoom-settings"), |handle| {
                                            let _ = handle.insert_idle(move |state| {
                                                state.spawn_command(
                                                    "cosmic-settings accessibility-magnifier"
                                                        .into(),
                                                );
                                            });
                                        }),
                                    ]
                                    .into_iter(),
                                    position.to_global(&output).to_i32_round(),
                                    MenuAlignment::horizontally_centered(
                                        (elem_size.h / 2.).round() as u32,
                                        false,
                                    ),
                                    Some(level.min(4.)),
                                    state.common.event_loop_handle.clone(),
                                    state.common.theme.clone(),
                                );

                                std::mem::drop(shell);
                                if grab.is_touch_grab() {
                                    seat.get_touch().unwrap().set_grab(state, grab, serial);
                                } else {
                                    seat.get_pointer().unwrap().set_grab(
                                        state,
                                        grab,
                                        serial,
                                        Focus::Clear,
                                    );
                                }
                            }
                        }
                    });
                }
            }
            ZoomMessage::Increment => {
                if let Some((seat, serial)) = last_seat.cloned() {
                    let increments = self.increments.clone();
                    let _ = loop_handle.insert_idle(move |state| {
                        if let Some(start_data) =
                            check_grab_preconditions(&seat, Some(serial), None)
                        {
                            let shell = state.common.shell.read();
                            let output = seat.active_output();

                            if shell.zoom_state().is_some() {
                                let output_state =
                                    output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
                                let output_state_ref = output_state.lock().unwrap();
                                let location = output_state_ref.global_pos_to_screen_space(
                                    start_data.location().as_global(),
                                    &output,
                                );

                                let output_geometry = output.geometry();

                                let elem_size =
                                    output_state_ref.element.current_size().to_f64().as_local();
                                let elem_location = Point::<f64, Local>::from((
                                    output_geometry.size.w as f64 / 2. - elem_size.w / 2.,
                                    output_geometry.size.h as f64 / 4. * 3. - elem_size.h / 2.,
                                ));
                                let position = Point::<_, Local>::from((
                                    location.x,
                                    elem_location.y + (elem_size.h / 2.),
                                ));
                                let level = output_state_ref.level;
                                std::mem::drop(output_state_ref);

                                let grab = MenuGrab::new(
                                    start_data,
                                    &seat,
                                    increments.into_iter().map(|val| {
                                        Item::new(format!("{}%", val), move |handle| {
                                            let _ = handle.insert_idle(move |state| {
                                                state
                                                    .common
                                                    .config
                                                    .cosmic_conf
                                                    .accessibility_zoom
                                                    .increment = val;
                                                state.common.update_config();
                                                if let Err(err) =
                                                    state.common.config.cosmic_helper.set(
                                                        "accessibility_zoom",
                                                        state
                                                            .common
                                                            .config
                                                            .cosmic_conf
                                                            .accessibility_zoom,
                                                    )
                                                {
                                                    error!(?err, "Failed to update zoom config");
                                                }
                                            });
                                        })
                                    }),
                                    position.to_global(&output).to_i32_round(),
                                    MenuAlignment::PREFER_CENTERED,
                                    Some(level.min(4.)),
                                    state.common.event_loop_handle.clone(),
                                    state.common.theme.clone(),
                                );

                                std::mem::drop(shell);
                                if grab.is_touch_grab() {
                                    seat.get_touch().unwrap().set_grab(state, grab, serial);
                                } else {
                                    seat.get_pointer().unwrap().set_grab(
                                        state,
                                        grab,
                                        serial,
                                        Focus::Clear,
                                    );
                                }
                            }
                        }
                    });
                }
            }
            ZoomMessage::Close => {
                let _ = loop_handle.insert_idle(|state| {
                    state
                        .common
                        .config
                        .cosmic_conf
                        .accessibility_zoom
                        .show_overlay = false;
                    if let Err(err) = state.common.config.cosmic_helper.set(
                        "accessibility_zoom",
                        state.common.config.cosmic_conf.accessibility_zoom,
                    ) {
                        error!(?err, "Failed to update zoom config");
                    }
                    state.common.update_config();
                });
            }
            ZoomMessage::Update {
                level,
                increment,
                movement,
            } => {
                self.level = level;
                self.movement = movement;

                if let Some(pos) = self.increments.iter().position(|val| *val == increment) {
                    self.increment_idx = pos;
                } else {
                    let mut increments = vec![25, 50, 100, 150, 200];
                    if !increments.contains(&increment) {
                        increments.push(increment);
                    }
                    increments.sort();
                    self.increment_idx =
                        increments.iter().position(|val| *val == increment).unwrap();
                    self.increments = increments;
                }
            }
        }
        cosmic::Task::none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ZoomFocusTarget {
    Main(ZoomElement),
    Menu(IcedElement<ContextMenu>),
}

impl From<ZoomElement> for ZoomFocusTarget {
    fn from(value: ZoomElement) -> Self {
        ZoomFocusTarget::Main(value)
    }
}

impl From<IcedElement<ContextMenu>> for ZoomFocusTarget {
    fn from(value: IcedElement<ContextMenu>) -> Self {
        ZoomFocusTarget::Menu(value)
    }
}

impl PointerTarget<State> for ZoomFocusTarget {
    fn enter(&self, seat: &Seat<State>, data: &mut State, event: &PointerMotionEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::enter(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => PointerTarget::enter(elem, seat, data, event),
        }
    }

    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &PointerMotionEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::motion(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => PointerTarget::motion(elem, seat, data, event),
        }
    }

    fn relative_motion(&self, seat: &Seat<State>, data: &mut State, event: &RelativeMotionEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::relative_motion(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => PointerTarget::relative_motion(elem, seat, data, event),
        }
    }

    fn button(&self, seat: &Seat<State>, data: &mut State, event: &ButtonEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::button(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => PointerTarget::button(elem, seat, data, event),
        }
    }

    fn axis(&self, seat: &Seat<State>, data: &mut State, frame: AxisFrame) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::axis(elem, seat, data, frame),
            ZoomFocusTarget::Menu(elem) => PointerTarget::axis(elem, seat, data, frame),
        }
    }

    fn frame(&self, seat: &Seat<State>, data: &mut State) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::frame(elem, seat, data),
            ZoomFocusTarget::Menu(elem) => PointerTarget::frame(elem, seat, data),
        }
    }

    fn gesture_swipe_begin(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GestureSwipeBeginEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_swipe_begin(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_swipe_begin(elem, seat, data, event)
            }
        }
    }

    fn gesture_swipe_update(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GestureSwipeUpdateEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_swipe_update(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_swipe_update(elem, seat, data, event)
            }
        }
    }

    fn gesture_swipe_end(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GestureSwipeEndEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_swipe_end(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_swipe_end(elem, seat, data, event)
            }
        }
    }

    fn gesture_pinch_begin(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GesturePinchBeginEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_pinch_begin(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_pinch_begin(elem, seat, data, event)
            }
        }
    }

    fn gesture_pinch_update(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GesturePinchUpdateEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_pinch_update(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_pinch_update(elem, seat, data, event)
            }
        }
    }

    fn gesture_pinch_end(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GesturePinchEndEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_pinch_end(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_pinch_end(elem, seat, data, event)
            }
        }
    }

    fn gesture_hold_begin(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        event: &GestureHoldBeginEvent,
    ) {
        match self {
            ZoomFocusTarget::Main(elem) => {
                PointerTarget::gesture_hold_begin(elem, seat, data, event)
            }
            ZoomFocusTarget::Menu(elem) => {
                PointerTarget::gesture_hold_begin(elem, seat, data, event)
            }
        }
    }

    fn gesture_hold_end(&self, seat: &Seat<State>, data: &mut State, event: &GestureHoldEndEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::gesture_hold_end(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => PointerTarget::gesture_hold_end(elem, seat, data, event),
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial, time: u32) {
        match self {
            ZoomFocusTarget::Main(elem) => PointerTarget::leave(elem, seat, data, serial, time),
            ZoomFocusTarget::Menu(elem) => PointerTarget::leave(elem, seat, data, serial, time),
        }
    }
}

impl TouchTarget<State> for ZoomFocusTarget {
    fn down(&self, seat: &Seat<State>, data: &mut State, event: &DownEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::down(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => TouchTarget::down(elem, seat, data, event),
        }
    }

    fn up(&self, seat: &Seat<State>, data: &mut State, event: &UpEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::up(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => TouchTarget::up(elem, seat, data, event),
        }
    }

    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &TouchMotionEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::motion(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => TouchTarget::motion(elem, seat, data, event),
        }
    }

    fn frame(&self, seat: &Seat<State>, data: &mut State, frame: FrameMarker) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::frame(elem, seat, data, frame),
            ZoomFocusTarget::Menu(elem) => TouchTarget::frame(elem, seat, data, frame),
        }
    }

    fn cancel(&self, seat: &Seat<State>, data: &mut State, frame: FrameMarker) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::cancel(elem, seat, data, frame),
            ZoomFocusTarget::Menu(elem) => TouchTarget::cancel(elem, seat, data, frame),
        }
    }

    fn shape(&self, seat: &Seat<State>, data: &mut State, event: &ShapeEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::shape(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => TouchTarget::shape(elem, seat, data, event),
        }
    }

    fn orientation(&self, seat: &Seat<State>, data: &mut State, event: &OrientationEvent) {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::orientation(elem, seat, data, event),
            ZoomFocusTarget::Menu(elem) => TouchTarget::orientation(elem, seat, data, event),
        }
    }

    fn last_frame(&self, seat: &Seat<State>, data: &mut State) -> Option<FrameMarker> {
        match self {
            ZoomFocusTarget::Main(elem) => TouchTarget::last_frame(elem, seat, data),
            ZoomFocusTarget::Menu(elem) => TouchTarget::last_frame(elem, seat, data),
        }
    }
}

impl IsAlive for ZoomFocusTarget {
    fn alive(&self) -> bool {
        match self {
            ZoomFocusTarget::Main(elem) => elem.alive(),
            ZoomFocusTarget::Menu(elem) => elem.alive(),
        }
    }
}
