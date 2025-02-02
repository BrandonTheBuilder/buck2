/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::borrow::Cow;
use std::io::Write;
use std::iter;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use buck2_data::CommandExecutionDetails;
use buck2_events::trace::TraceId;
use buck2_events::BuckEvent;
use dupe::Dupe;
use gazebo::prelude::*;
use superconsole::components::splitting::SplitKind;
use superconsole::components::Bounded;
use superconsole::components::Split;
use superconsole::content::colored_lines_from_multiline_string;
use superconsole::content::lines_from_multiline_string;
use superconsole::content::LinesExt;
use superconsole::style::Attribute;
use superconsole::style::Color;
use superconsole::style::ContentStyle;
use superconsole::style::StyledContent;
use superconsole::style::Stylize;
use superconsole::Component;
use superconsole::Dimensions;
use superconsole::Direction;
use superconsole::DrawMode;
use superconsole::Line;
use superconsole::Lines;
use superconsole::Span;
use superconsole::State;
pub(crate) use superconsole::SuperConsole;

use crate::subscribers::display;
use crate::subscribers::display::display_file_watcher_end;
use crate::subscribers::display::TargetDisplayOptions;
use crate::subscribers::io::IoHeader;
use crate::subscribers::simpleconsole::SimpleConsole;
use crate::subscribers::subscriber::Tick;
use crate::subscribers::subscriber_unpack::UnpackingEventSubscriber;
use crate::subscribers::superconsole::commands::CommandsComponent;
use crate::subscribers::superconsole::commands::CommandsComponentState;
use crate::subscribers::superconsole::debug_events::DebugEventsComponent;
use crate::subscribers::superconsole::debug_events::DebugEventsState;
use crate::subscribers::superconsole::dice::DiceComponent;
use crate::subscribers::superconsole::dice::DiceState;
use crate::subscribers::superconsole::re::ReHeader;
use crate::subscribers::superconsole::test::TestState;
use crate::subscribers::superconsole::timed_list::Cutoffs;
use crate::subscribers::superconsole::timed_list::TimedList;
use crate::verbosity::Verbosity;
use crate::what_ran;
use crate::what_ran::local_command_to_string;
use crate::what_ran::WhatRanOptions;

mod commands;
mod common;
pub(crate) mod debug_events;
pub(crate) mod dice;
mod re;
pub mod test;
pub mod timed_list;

pub const SUPERCONSOLE_WIDTH: usize = 150;

/// Information about the current command, such as session or build ids.
#[derive(Default)]
pub(crate) struct SessionInfo {
    trace_id: Option<TraceId>,
    test_session: Option<buck2_data::TestSessionInfo>,
}

pub const CUTOFFS: Cutoffs = Cutoffs {
    inform: Duration::from_secs(4),
    warn: Duration::from_secs(8),
    _notable: Duration::from_millis(200),
};
const MAX_EVENTS: usize = 10;

pub struct StatefulSuperConsole {
    state: SuperConsoleState,
    super_console: Option<SuperConsole>,
    verbosity: Verbosity,
}

#[derive(Copy, Clone, Dupe)]
struct TimeSpeed {
    speed: f64,
}

const TIMESPEED_DEFAULT: f64 = 1.0;

impl TimeSpeed {
    pub(crate) fn new(speed_value: Option<f64>) -> anyhow::Result<Self> {
        let speed = speed_value.unwrap_or(TIMESPEED_DEFAULT);

        if speed <= 0.0 {
            return Err(anyhow::anyhow!("Time speed cannot be negative!"));
        }
        Ok(TimeSpeed { speed })
    }

    pub(crate) fn speed(self) -> f64 {
        self.speed
    }
}

#[derive(Default)]
pub(crate) struct TimedListState {
    /// Two lines for root events with single child event.
    pub(crate) two_lines: bool,
}

pub(crate) struct SuperConsoleState {
    test_state: TestState,
    current_tick: Tick,
    session_info: SessionInfo,
    time_speed: TimeSpeed,
    dice_state: DiceState,
    debug_events: DebugEventsState,
    commands_state: CommandsComponentState,
    /// This contains the SpanTracker, which is why it's part of the SuperConsoleState.
    simple_console: SimpleConsole,
    timed_list: TimedListState,
}

#[derive(Default)]
pub struct SuperConsoleConfig {
    // Offer a spot to put components between the banner and the timed list using `sandwiched`.
    pub(crate) sandwiched: Option<Box<dyn Component>>,
    pub(crate) enable_dice: bool,
    pub(crate) enable_debug_events: bool,
}

impl StatefulSuperConsole {
    pub(crate) fn default_layout(
        command_name: &str,
        config: SuperConsoleConfig,
    ) -> Box<dyn Component> {
        let header = format!("Command: `{}`.", command_name);
        let mut components: Vec<Box<dyn Component>> =
            vec![box SessionInfoComponent, ReHeader::boxed(), box IoHeader];
        if let Some(sandwiched) = config.sandwiched {
            components.push(sandwiched);
        }
        components.push(box DebugEventsComponent);
        components.push(box DiceComponent);
        components.push(box CommandsComponent);
        components.push(box TimedList::new(MAX_EVENTS, CUTOFFS, header));
        let root = box Split::new(components, Direction::Vertical, SplitKind::Adaptive);
        // bound all components to our recommended grapheme-width
        box Bounded::new(root, Some(SUPERCONSOLE_WIDTH), None)
    }

    pub fn new_with_root_forced(
        root: Box<dyn Component>,
        verbosity: Verbosity,
        show_waiting_message: bool,
        replay_speed: Option<f64>,
        stream: Option<Box<dyn Write + Send + 'static + Sync>>,
        config: SuperConsoleConfig,
    ) -> anyhow::Result<Self> {
        let fallback_size = ::superconsole::Dimensions {
            width: 100,
            height: 40,
        };
        let mut builder = Self::console_builder();
        if let Some(stream) = stream {
            builder.write_to(stream);
        }
        Self::new(
            builder.build_forced(root, fallback_size)?,
            verbosity,
            show_waiting_message,
            replay_speed,
            config,
        )
    }

    pub(crate) fn new_with_root(
        root: Box<dyn Component>,
        verbosity: Verbosity,
        show_waiting_message: bool,
        replay_speed: Option<f64>,
        config: SuperConsoleConfig,
    ) -> anyhow::Result<Option<Self>> {
        match Self::console_builder().build(root)? {
            None => Ok(None),
            Some(sc) => Ok(Some(Self::new(
                sc,
                verbosity,
                show_waiting_message,
                replay_speed,
                config,
            )?)),
        }
    }

    pub(crate) fn new(
        super_console: SuperConsole,
        verbosity: Verbosity,
        show_waiting_message: bool,
        replay_speed: Option<f64>,
        config: SuperConsoleConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            state: SuperConsoleState {
                test_state: TestState::default(),
                current_tick: Tick::now(),
                session_info: SessionInfo::default(),
                time_speed: TimeSpeed::new(replay_speed)?,
                simple_console: SimpleConsole::with_tty(verbosity, show_waiting_message),
                dice_state: DiceState::new(config.enable_dice),
                debug_events: DebugEventsState::new(config.enable_debug_events),
                commands_state: CommandsComponentState { enabled: false },
                timed_list: TimedListState::default(),
            },
            super_console: Some(super_console),
            verbosity,
        })
    }

    /// Construct a console suitable for use by the Buck2 CLI. We use non-blocking output here
    /// because we do all our event processing on a single thread, so that if stderr is blocked
    /// (e.g.  because the client is using a resumable remote terminal and they've temporarily
    /// disconnected), we don't delay ingesting new events.
    ///
    /// This ensures we a) don't have to catch up when the client reconnects, b) don't buffer
    /// events (though we might buffer output), and c) can show an accurate "time elapsed" when the
    /// client returns (if we wait until the client returns to resume, we'll always report that the
    /// command finished "just now", because we'll have some events to catch up on).
    fn console_builder() -> ::superconsole::Builder {
        let mut builder = ::superconsole::Builder::new();
        builder.non_blocking();
        builder
    }

    /// Render the console for a final time, but use the Normal draw mode.
    /// Fails if there isn't a superconsole.
    pub fn render_final_normal_console(self) -> anyhow::Result<()> {
        match self.super_console {
            Some(sc) => sc.finalize_with_mode(&self.state.state(), DrawMode::Normal),
            None => Err(anyhow::anyhow!("Cannot render non-existent superconsole")),
        }
    }
}

impl SuperConsoleState {
    // Collect all state to send to super console. Note that the SpanTracker state is held in the
    // SimpleConsole so that if we downgrade to the SimpleConsole, we don't lose tracked spans.
    pub(crate) fn state(&self) -> superconsole::State {
        superconsole::state![
            self.simple_console.spans(),
            self.simple_console.action_stats(),
            &self.test_state,
            &self.session_info,
            &self.current_tick,
            &self.time_speed,
            &self.dice_state,
            self.simple_console.re_panel(),
            &self.simple_console.io_state,
            &self.debug_events,
            &self.commands_state,
            &self.timed_list,
        ]
    }
}

impl StatefulSuperConsole {
    async fn toggle(
        &mut self,
        what: &str,
        key: char,
        var: impl FnOnce(&mut Self) -> &mut bool,
    ) -> anyhow::Result<()> {
        let var = var(self);
        *var = !*var;
        let on_off = match *var {
            true => "on",
            false => "off",
        };
        self.handle_stderr(&format!("{what}: {on_off}, press `{key}` to revert"))
            .await
    }
}

// TODO(brasselsprouts): after deprecating filetailers, simplify these code paths
#[async_trait]
impl UnpackingEventSubscriber for StatefulSuperConsole {
    async fn handle_event(&mut self, event: &Arc<BuckEvent>) -> anyhow::Result<()> {
        match &mut self.super_console {
            Some(_) => {
                self.handle_inner_event(event)
                    .await
                    .with_context(|| display::InvalidBuckEvent(event.clone()))?;
                self.state.simple_console.update_span_tracker(event)?;
            }
            None => {
                self.state.simple_console.handle_event(event).await?;
            }
        }

        self.state
            .debug_events
            .handle_event(self.state.current_tick.start_time, event)?;

        if self.verbosity.print_all_commands() {
            // This is a bit messy. It would be better for this to go in the branch above, but we
            // can't do that, because we call a method on `self` in a branch that takes a mutable
            // borrow of the SuperConsole there. That works *only* if we don't use the console we
            // borrowed.
            if let Some(console) = &mut self.super_console {
                what_ran::emit_event_if_relevant(
                    event.parent_id().into(),
                    event.data(),
                    self.state.simple_console.spans(),
                    console,
                    &WhatRanOptions::default(),
                )?;
            }
        }

        Ok(())
    }

    async fn handle_stderr(&mut self, msg: &str) -> anyhow::Result<()> {
        match &mut self.super_console {
            Some(super_console) => {
                super_console.emit(msg.lines().map(Line::sanitized).collect());
                Ok(())
            }
            None => self.state.simple_console.handle_stderr(msg).await,
        }
    }

    async fn handle_file_watcher_end(
        &mut self,
        file_watcher: &buck2_data::FileWatcherEnd,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        match &mut self.super_console {
            Some(super_console) => {
                super_console
                    .emit(display_file_watcher_end(file_watcher).into_map(|x| Line::sanitized(&x)));
                Ok(())
            }
            None => {
                self.state
                    .simple_console
                    .handle_file_watcher_end(file_watcher, event)
                    .await
            }
        }
    }

    async fn handle_output(&mut self, raw_output: &str) -> anyhow::Result<()> {
        if let Some(super_console) = self.super_console.take() {
            super_console.finalize(&self.state.state())?;
        }

        self.state.simple_console.handle_output(raw_output).await
    }

    async fn handle_console_interaction(&mut self, c: char) -> anyhow::Result<()> {
        if c == 'd' {
            self.toggle("DICE component", 'd', |s| &mut s.state.dice_state.enabled)
                .await?;
        } else if c == 'e' {
            self.toggle("Debug events component", 'e', |s| {
                &mut s.state.debug_events.enabled
            })
            .await?;
        } else if c == '2' {
            self.toggle("Two lines mode", '2', |s| &mut s.state.timed_list.two_lines)
                .await?;
        } else if c == 'r' {
            self.toggle("Detailed RE", 'r', |s| {
                &mut s.state.simple_console.re_panel_mut().detailed
            })
            .await?;
        } else if c == 'i' {
            self.toggle("I/O counters", 'i', |s| {
                &mut s.state.simple_console.io_state.enabled
            })
            .await?;
        } else if c == 'c' {
            self.toggle("Commands", 'c', |s| &mut s.state.commands_state.enabled)
                .await?;
        } else if c == '?' || c == 'h' {
            self.handle_stderr(
                "Help:\n\
                `d` = toggle DICE\n\
                `e` = toggle debug events\n\
                `2` = toggle two lines mode\n\
                `r` = toggle detailed RE\n\
                `i` = toggle I/O counters\n\
                `h` = show this help",
            )
            .await?;
        }

        Ok(())
    }

    async fn handle_command_start(
        &mut self,
        _command: &buck2_data::CommandStart,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.state.session_info.trace_id = Some(event.trace_id()?);
        Ok(())
    }

    async fn handle_command_result(
        &mut self,
        result: &buck2_cli_proto::CommandResult,
    ) -> anyhow::Result<()> {
        match self.super_console.take() {
            Some(mut super_console) => {
                if let buck2_cli_proto::CommandResult {
                    result: Some(buck2_cli_proto::command_result::Result::Error(e)),
                } = result
                {
                    let style = ContentStyle {
                        foreground_color: Some(Color::DarkRed),
                        ..Default::default()
                    };
                    for message in &e.messages {
                        let lines = lines_from_multiline_string(message, style);
                        super_console.emit(lines);
                    }
                }
                super_console.finalize(&self.state.state())
            }
            None => {
                self.state
                    .simple_console
                    .handle_command_result(result)
                    .await
            }
        }
    }

    async fn tick(&mut self, tick: &Tick) -> anyhow::Result<()> {
        self.state.simple_console.detect_hangs().await?;
        match &mut self.super_console {
            Some(super_console) => {
                self.state.current_tick = tick.dupe();
                super_console.render(&self.state.state())
            }
            None => Ok(()),
        }
    }

    async fn handle_error(&mut self, _error: &anyhow::Error) -> anyhow::Result<()> {
        match self.super_console.take() {
            Some(super_console) => super_console.finalize(&self.state.state()),
            None => Ok(()),
        }
    }

    async fn handle_re_session_created(
        &mut self,
        session: &buck2_data::RemoteExecutionSessionCreated,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.state
            .simple_console
            .re_panel_mut()
            .add_re_session(session);
        Ok(())
    }

    async fn handle_console_message(
        &mut self,
        message: &buck2_data::ConsoleMessage,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        // TODO(nmj): Maybe better handling of messages that have color data in them. Right now
        //            they're just stripped
        match &mut self.super_console {
            Some(super_console) => {
                super_console.emit(lines_from_multiline_string(
                    &message.message,
                    ContentStyle::default(),
                ));
                Ok(())
            }
            None => {
                self.state
                    .simple_console
                    .handle_console_message(message, event)
                    .await
            }
        }
    }

    async fn handle_action_execution_end(
        &mut self,
        action: &buck2_data::ActionExecutionEnd,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.state.simple_console.action_stats_mut().update(action);

        let super_console = match &mut self.super_console {
            Some(super_console) => super_console,
            None => {
                return self
                    .state
                    .simple_console
                    .handle_action_execution_end(action, event)
                    .await;
            }
        };

        let mut lines = vec![];

        match action.error.as_ref() {
            Some(error) => {
                let display::ActionErrorDisplay {
                    action_id,
                    reason,
                    command,
                } = display::display_action_error(
                    action,
                    error,
                    TargetDisplayOptions::for_console(),
                )?;

                lines.push(Line::from_iter([Span::new_styled_lossy(
                    StyledContent::new(
                        ContentStyle {
                            foreground_color: Some(Color::White),
                            attributes: Attribute::Bold.into(),
                            ..Default::default()
                        },
                        format!("Action failed: {}", action_id,),
                    ),
                )]));

                lines.push(Line::from_iter([Span::new_styled_lossy(
                    reason.with(Color::DarkRed),
                )]));

                if let Some(command) = command {
                    lines_for_command_details(&command, self.verbosity, &mut lines);
                }
            }
            None => {
                if let Some(stderr) = display::success_stderr(action, self.verbosity)? {
                    let action_id = StyledContent::new(
                        ContentStyle {
                            foreground_color: Some(Color::White),
                            attributes: Attribute::Bold.into(),
                            ..Default::default()
                        },
                        format!(
                            "stderr for {}:",
                            display::display_action_identity(
                                action.key.as_ref(),
                                action.name.as_ref(),
                                TargetDisplayOptions::for_console(),
                            )?
                        ),
                    );
                    lines.push(Line::from_iter([Span::new_styled_lossy(action_id)]));
                    lines.extend(colored_lines_from_multiline_string(stderr));
                }
            }
        }

        super_console.emit(lines);

        Ok(())
    }

    async fn handle_test_discovery(
        &mut self,
        test_info: &buck2_data::TestDiscovery,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        if let Some(data) = &test_info.data {
            match data {
                buck2_data::test_discovery::Data::Session(session_info) => {
                    self.state.session_info.test_session = Some(session_info.clone());
                }
                buck2_data::test_discovery::Data::Tests(tests) => {
                    self.state.test_state.discovered += tests.test_names.len() as u64
                }
            }
        }

        Ok(())
    }

    async fn handle_test_result(
        &mut self,
        result: &buck2_data::TestResult,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.state.test_state.update(result)?;
        if let Some(super_console) = &mut self.super_console {
            if let Some(msg) = display::format_test_result(result)? {
                super_console.emit(msg);
            }
        }

        Ok(())
    }

    async fn handle_dice_snapshot(
        &mut self,
        update: &buck2_data::DiceStateSnapshot,
    ) -> anyhow::Result<()> {
        self.state.dice_state.update(update);
        Ok(())
    }

    async fn handle_snapshot(
        &mut self,
        update: &buck2_data::Snapshot,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.state
            .simple_console
            .handle_snapshot(update, event)
            .await
    }
}

fn lines_for_command_details(
    command_failed: &CommandExecutionDetails,
    verbosity: Verbosity,
    lines: &mut Vec<Line>,
) {
    use buck2_data::command_execution_details::Command;

    match command_failed.command.as_ref() {
        Some(Command::LocalCommand(local_command)) => {
            let command = local_command_to_string(local_command);
            let command = command.as_str();
            let command = if verbosity.print_failure_full_command() {
                Cow::Borrowed(command)
            } else {
                match truncate(command) {
                    None => Cow::Borrowed(command),
                    Some(short) => Cow::Owned(format!(
                        "{} (run `buck2 log what-failed` to get the full command)",
                        short
                    )),
                }
            };

            lines.push(Line::from_iter([Span::new_styled_lossy(
                format!("Reproduce locally: `{}`", command).with(Color::DarkRed),
            )]));
        }
        Some(Command::RemoteCommand(remote_command)) => {
            lines.push(Line::from_iter([Span::new_styled_lossy(
                format!(
                    "Reproduce locally: `frecli cas download-action {}`",
                    remote_command.action_digest
                )
                .with(Color::DarkRed),
            )]));
        }
        Some(Command::OmittedLocalCommand(..)) | None => {
            // Nothing to show in this case.
        }
    };

    lines.push(Line::from_iter([Span::new_styled_lossy(
        "stdout:"
            .to_owned()
            .with(Color::DarkRed)
            .attribute(Attribute::Bold),
    )]));
    lines.extend(lines_from_multiline_string(
        &command_failed.stdout,
        color(Color::DarkRed),
    ));
    lines.push(Line::from_iter([Span::new_styled_lossy(
        "stderr:"
            .to_owned()
            .with(Color::DarkRed)
            .attribute(Attribute::Bold),
    )]));
    lines.extend(colored_lines_from_multiline_string(&command_failed.stderr));
}

// Truncates a string to a reasonable number characters, or returns None if it doesn't need truncating.
fn truncate(contents: &str) -> Option<String> {
    const MAX_LENGTH: usize = 200;
    const BUFFER: usize = " ...<omitted>... ".len();
    if contents.len() > MAX_LENGTH + BUFFER {
        Some(format!(
            "{} ...<omitted>... {}",
            &contents[0..MAX_LENGTH / 2],
            &contents[contents.len() - MAX_LENGTH / 2..contents.len()]
        ))
    } else {
        None
    }
}

fn color(color: Color) -> ContentStyle {
    ContentStyle {
        foreground_color: Some(color),
        ..Default::default()
    }
}

/// This component is used to display session information for a command e.g. RE session ID
#[derive(Debug)]
pub struct SessionInfoComponent;

impl Component for SessionInfoComponent {
    fn draw_unchecked(
        &self,
        state: &State,
        dimensions: Dimensions,
        _mode: DrawMode,
    ) -> anyhow::Result<Lines> {
        match state.get::<SessionInfo>() {
            Ok(session_info) => {
                let mut headers = vec![];
                let mut ids = vec![];
                if let Some(trace_id) = &session_info.trace_id {
                    if cfg!(fbcode_build) {
                        headers.push(Line::unstyled("Buck UI:")?);
                        ids.push(Span::new_unstyled(format!(
                            "https://www.internalfb.com/buck2/{}",
                            trace_id
                        ))?);
                    } else {
                        headers.push(Line::unstyled("Build ID:")?);
                        ids.push(Span::new_unstyled(trace_id)?);
                    }
                }
                if let Some(buck2_data::TestSessionInfo { info }) = &session_info.test_session {
                    headers.push(Line::unstyled("Test UI:")?);
                    ids.push(Span::new_unstyled(info)?);
                }
                // pad all headers to the max width.
                headers.justify();
                headers.pad_lines_right(1);

                let max_len = headers
                    .iter()
                    .zip(ids.iter())
                    .map(|(header, id)| header.len() + id.len())
                    .max()
                    .unwrap_or(0);

                let lines = if max_len > dimensions.width {
                    headers
                        .into_iter()
                        .zip(ids.into_iter())
                        .flat_map(|(header, id)| {
                            iter::once(header).chain(iter::once(Line(vec![id])))
                        })
                        .collect()
                } else {
                    headers
                        .iter_mut()
                        .zip(ids.into_iter())
                        .for_each(|(header, id)| header.0.push(id));
                    headers
                };

                let max_len = lines.iter().map(|line| line.len()).max().unwrap_or(0);

                Ok(if max_len > dimensions.width {
                    vec![Line::unstyled("<Terminal too small for build details>")?]
                } else {
                    lines
                })
            }
            Err(_) => Ok(vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use buck2_cli_proto::CommandResult;
    use buck2_cli_proto::GenericResponse;
    use buck2_data::LoadBuildFileEnd;
    use buck2_data::LoadBuildFileStart;
    use buck2_data::SpanEndEvent;
    use buck2_data::SpanStartEvent;
    use buck2_events::span::SpanId;
    use superconsole::testing::frame_contains;
    use superconsole::testing::test_console;
    use superconsole::testing::SuperConsoleTestingExt;

    use super::*;

    #[tokio::test]
    async fn test_transfer_state_to_simpleconsole() {
        let mut console = StatefulSuperConsole::new_with_root_forced(
            StatefulSuperConsole::default_layout("test", SuperConsoleConfig::default()),
            Verbosity::Default,
            true,
            None,
            None,
            Default::default(),
        )
        .unwrap();

        // start a new event.
        let id = SpanId::new();
        let event = Arc::new(BuckEvent::new(
            SystemTime::now(),
            TraceId::new(),
            Some(id),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Load(
                    LoadBuildFileStart {
                        module_id: "foo".to_owned(),
                        cell: "bar".to_owned(),
                    },
                )),
            }),
        ));
        console.handle_event(&event).await.unwrap();

        // drop into simple console
        console
            .handle_command_result(&CommandResult {
                result: Some(buck2_cli_proto::command_result::Result::GenericResponse(
                    GenericResponse {},
                )),
            })
            .await
            .unwrap();

        // finish the event from before
        // expect to successfully close event.
        let event = Arc::new(BuckEvent::new(
            SystemTime::now(),
            TraceId::new(),
            Some(id),
            None,
            buck2_data::buck_event::Data::SpanEnd(SpanEndEvent {
                data: Some(buck2_data::span_end_event::Data::Load(LoadBuildFileEnd {
                    module_id: "foo".to_owned(),
                    cell: "bar".to_owned(),
                    error: None,
                })),
                stats: None,
                duration: None,
            }),
        ));
        assert!(console.handle_event(&event).await.is_ok());
    }

    #[tokio::test]
    async fn test_default_layout() -> anyhow::Result<()> {
        let trace_id = TraceId::new();
        let now = SystemTime::now();
        let tick = Tick::now();

        let mut console = StatefulSuperConsole::new(
            test_console(StatefulSuperConsole::default_layout(
                "build",
                Default::default(),
            )),
            Verbosity::Default,
            true,
            Default::default(),
            Default::default(),
        )?;

        console
            .handle_event(&Arc::new(BuckEvent::new(
                now,
                trace_id.dupe(),
                Some(SpanId::new()),
                None,
                buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                    data: Some(
                        buck2_data::CommandStart {
                            metadata: Default::default(),
                            data: Some(buck2_data::BuildCommandStart {}.into()),
                        }
                        .into(),
                    ),
                }),
            )))
            .await?;

        console
            .handle_event(&Arc::new(BuckEvent::new(
                now,
                trace_id.dupe(),
                None,
                None,
                buck2_data::InstantEvent {
                    data: Some(
                        buck2_data::RemoteExecutionSessionCreated {
                            session_id: "reSessionID-123".to_owned(),
                            experiment_name: "".to_owned(),
                        }
                        .into(),
                    ),
                }
                .into(),
            )))
            .await?;

        console
            .handle_event(&Arc::new(BuckEvent::new(
                now,
                trace_id.dupe(),
                Some(SpanId::new()),
                None,
                SpanStartEvent {
                    data: Some(
                        LoadBuildFileStart {
                            module_id: "foo".to_owned(),
                            cell: "bar".to_owned(),
                        }
                        .into(),
                    ),
                }
                .into(),
            )))
            .await?;

        console.tick(&tick).await?;

        let frame = console
            .super_console
            .as_mut()
            .context("Console was downgraded")?
            .test_output_mut()?
            .frames
            .pop()
            .context("No frame was emitted")?;

        // Verify we have the right output on intermediate frames
        if cfg!(fbcode_build) {
            assert!(frame_contains(&frame, "Buck UI:"));
        } else {
            assert!(frame_contains(&frame, "Build ID:"));
        }
        assert!(frame_contains(&frame, "RE: reSessionID-123"));
        assert!(frame_contains(&frame, "In progress"));

        console
            .handle_command_result(&buck2_cli_proto::CommandResult { result: None })
            .await?;

        Ok(())
    }

    #[test]
    fn test_session_info() -> anyhow::Result<()> {
        let info = SessionInfo {
            trace_id: Some(TraceId::null()),
            test_session: Some(buck2_data::TestSessionInfo {
                info: (0..100).map(|_| "a").collect(),
            }),
        };
        let state = superconsole::state![&info];

        let full = SessionInfoComponent.draw_unchecked(
            &state,
            Dimensions {
                // Enough to print everything on one line (we need 109 in fbcode and 110 in OSS)
                width: 110,
                height: 1,
            },
            DrawMode::Normal,
        )?;

        assert_eq!(full.len(), 2);

        let multiline = SessionInfoComponent.draw_unchecked(
            &state,
            Dimensions {
                // Just long enough to print each on one line.
                width: 100,
                height: 1,
            },
            DrawMode::Normal,
        )?;

        assert_eq!(multiline.len(), 4);

        let too_small = SessionInfoComponent.draw_unchecked(
            &state,
            Dimensions {
                width: 1,
                height: 1,
            },
            DrawMode::Normal,
        )?;

        assert_eq!(too_small.len(), 1);

        Ok(())
    }
}
