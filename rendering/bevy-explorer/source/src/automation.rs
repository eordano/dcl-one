//! Console commands end-to-end tests and agents drive the client with
//! (docs/guidelines/running.md, "Test commands"):
//!
//! - [`AutomationPlugin`]: `/press_key` and `/click_at`, which feed the engine the same input
//!   events the window would send, so both the world and the CEF HUD (which gets every key and
//!   every unlocked-cursor mouse event, react_hud_cef.rs) see them. On the web the browser, not
//!   the engine, delivers input, so react-web's `lib/hudAutomation.ts` answers these two commands
//!   in the page.
//! - [`pointer`], in the same plugin: `/hover_entity` and `/click_entity`, which hover and click
//!   a scene entity by id through the pointer pipeline, on native and web alike.
//! - [`remote`]: the loopback server that runs any console command from outside the process, for
//!   callers holding the run's token, started by [`AutomationOptions`] (`--automation-token`).
//!
//! `/world_state` is `world_state.rs`, which the headless client has too. [`AutomationPlugin`]
//! runs on every target; [`remote`] is native only (lib.rs).

use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use console::DoAddConsoleCommand;
use system_api_types::launch_options::help_heading::AUTOMATION;

/// The native flags of the loopback console server (`remote`), under the `AUTOMATION` help
/// heading: the server starts only when a token is given. The headless client takes them too.
#[derive(clap::Args, Default, Clone, Debug, PartialEq)]
pub struct AutomationOptions {
    /// The token every call to the server must carry; the server starts only with one. Absent =
    /// the `DCL_AUTOMATION_TOKEN` environment variable
    #[arg(long, value_name = "token", help_heading = AUTOMATION)]
    pub automation_token: Option<String>,

    /// The port the server listens on, on 127.0.0.1; with a token only. Absent = the
    /// `DCL_AUTOMATION_PORT` environment variable, else 15702
    #[arg(long, value_name = "port", help_heading = AUTOMATION)]
    pub automation_port: Option<u16>,
}

#[cfg(feature = "remote")]
impl AutomationOptions {
    const TOKEN_ENV: &'static str = "DCL_AUTOMATION_TOKEN";
    const PORT_ENV: &'static str = "DCL_AUTOMATION_PORT";

    /// The server these options ask for, if they do.
    pub fn server(&self) -> Option<remote::ConsoleServerPlugin> {
        self.server_from(
            std::env::var(Self::TOKEN_ENV).ok(),
            std::env::var(Self::PORT_ENV).ok(),
        )
    }

    /// The flags, else the environment's values: a server needs a non-empty token.
    fn server_from(
        &self,
        env_token: Option<String>,
        env_port: Option<String>,
    ) -> Option<remote::ConsoleServerPlugin> {
        let token = self
            .automation_token
            .clone()
            .or(env_token)
            .filter(|token| !token.is_empty());
        let Some(token) = token else {
            if self.automation_port.is_some() {
                warn!(
                    "--automation-port has no effect without --automation-token: no console server"
                );
            }
            return None;
        };
        let port = self
            .automation_port
            .or_else(|| {
                let port = env_port?;
                port.parse()
                    .inspect_err(|_| {
                        warn!(
                            "{} `{port}` is not a port number: using the default",
                            Self::PORT_ENV
                        )
                    })
                    .ok()
            })
            .unwrap_or(bevy::remote::http::DEFAULT_PORT);
        Some(remote::ConsoleServerPlugin { port, token })
    }
}

/// `/press_key`, `/click_at`, `/hover_entity` and `/click_entity`, in every build.
pub struct AutomationPlugin;

impl Plugin for AutomationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<input::InputScript>()
            .add_console_command::<input::PressKeyCommand, _>(input::press_key_cmd)
            .add_console_command::<input::ClickAtCommand, _>(input::click_at_cmd)
            .add_systems(
                PreUpdate,
                input::play_input_script.before(bevy::input::InputSystem),
            );
        app.add_plugins(pointer::AutomationPointerPlugin);
    }
}

pub(crate) mod pointer;

pub(crate) mod input {
    use std::collections::VecDeque;

    use super::*;
    use bevy::input::{
        keyboard::{Key, KeyboardInput, NativeKeyCode},
        mouse::MouseButtonInput,
        ButtonState,
    };
    use bevy::window::PrimaryWindow;
    use common::rpc::RpcResultSender;
    use console::PendingConsoleResponses;

    /// Input queued by `/press_key`, `/click_at`, `/click_entity` and the standalone's
    /// `/hud_fill`, one step a frame so each lands as a frame of its own, the way a person's key or
    /// click would. Steps are numbered from 1 in the order they are queued; [`Self::played`] counts
    /// the steps played so far, so a caller can tell when its step has landed.
    #[derive(Resource, Default)]
    pub struct InputScript {
        steps: VecDeque<Step>,
        queued: u64,
        played: u64,
    }

    impl InputScript {
        /// Queue a step after those already queued, and return its number: the step has been
        /// played once [`Self::played`] reaches it.
        pub(crate) fn push(&mut self, step: Step) -> u64 {
            self.steps.push_back(step);
            self.queued += 1;
            self.queued
        }

        /// How many steps have been played (each in the `PreUpdate` of its frame).
        pub(crate) fn played(&self) -> u64 {
            self.played
        }

        fn pop(&mut self) -> Option<Step> {
            let step = self.steps.pop_front()?;
            self.played += 1;
            Some(step)
        }

        #[cfg(test)]
        pub fn steps(&self) -> usize {
            self.steps.len()
        }
    }

    /// Type `text` a key at a time, as a keyboard would: each character's press carries it as its
    /// text, on a key code that binds nothing, so the world acts on none of it. `reply` answers
    /// with `message` once the last key is up. For `/hud_fill` (react_hud_cef/automation.rs).
    #[cfg_attr(not(feature = "react-hud-cef"), allow(dead_code))]
    pub fn queue_typing(
        script: &mut InputScript,
        text: &str,
        reply: RpcResultSender<Result<String, String>>,
        message: String,
    ) {
        for c in text.chars() {
            let (key_code, logical_key, text) = match c {
                '\n' => (KeyCode::Enter, Key::Enter, "\r".to_owned()),
                c => (
                    KeyCode::Unidentified(NativeKeyCode::Unidentified),
                    Key::Character(c.to_string().as_str().into()),
                    c.to_string(),
                ),
            };
            for state in [ButtonState::Pressed, ButtonState::Released] {
                script.push(Step::Key {
                    key_code,
                    logical_key: logical_key.clone(),
                    text: Some(text.clone()),
                    state,
                });
            }
        }
        script.push(Step::Done(reply, message));
    }

    pub enum Step {
        Key {
            key_code: KeyCode,
            logical_key: Key,
            text: Option<String>,
            state: ButtonState,
        },
        Cursor(Vec2),
        Button(MouseButton, ButtonState),
        Done(RpcResultSender<Result<String, String>>, String),
    }

    /// Before bevy's input systems, so the events are read in the frame they are written.
    pub fn play_input_script(
        mut script: ResMut<InputScript>,
        mut windows: Query<(Entity, &mut Window), With<PrimaryWindow>>,
        mut keys: EventWriter<KeyboardInput>,
        mut buttons: EventWriter<MouseButtonInput>,
    ) {
        let Some(step) = script.pop() else {
            return;
        };
        let Ok((window_entity, mut window)) = windows.single_mut() else {
            if let Step::Done(reply, _) = step {
                reply.send(Err("no primary window".to_owned()));
            }
            return;
        };
        match step {
            Step::Key {
                key_code,
                logical_key,
                text,
                state,
            } => {
                keys.write(KeyboardInput {
                    key_code,
                    logical_key,
                    state,
                    text: text
                        .filter(|_| state == ButtonState::Pressed)
                        .map(Into::into),
                    repeat: false,
                    window: window_entity,
                });
            }
            Step::Cursor(position) => window.set_cursor_position(Some(position)),
            Step::Button(button, state) => {
                buttons.write(MouseButtonInput {
                    button,
                    state,
                    window: window_entity,
                });
            }
            Step::Done(reply, message) => reply.send(Ok(message)),
        }
    }

    // --- /press_key ---

    /// Press a key as the keyboard would: the world and the HUD both get it. The key is named as
    /// DOM `KeyboardEvent.code` names it (`KeyB`, `Digit1`, `Space`, `Enter`, `Escape`,
    /// `ArrowUp`, `F1`, ...)
    #[derive(clap::Parser, ConsoleCommand)]
    #[command(name = "/press_key")]
    pub struct PressKeyCommand {
        /// the key's code name
        code: String,
        /// `down` presses without releasing, `up` releases; a press and release by default
        #[arg(long)]
        state: Option<String>,
    }

    pub fn press_key_cmd(
        mut input: ConsoleCommand<PressKeyCommand>,
        mut script: ResMut<InputScript>,
        mut pending: ResMut<PendingConsoleResponses>,
    ) {
        let Some(Ok(command)) = input.take() else {
            return;
        };
        let Some((key_code, logical_key, text)) = key_from_code(&command.code) else {
            input.reply_failed(format!("unknown key code `{}`", command.code));
            return;
        };
        let states: &[ButtonState] = match command.state.as_deref() {
            None => &[ButtonState::Pressed, ButtonState::Released],
            Some("down") => &[ButtonState::Pressed],
            Some("up") => &[ButtonState::Released],
            Some(other) => {
                input.reply_failed(format!("unknown state `{other}`: use down or up"));
                return;
            }
        };
        for state in states {
            script.push(Step::Key {
                key_code,
                logical_key: logical_key.clone(),
                text: text.clone(),
                state: *state,
            });
        }
        let (reply, rx) = RpcResultSender::channel();
        script.push(Step::Done(reply, format!("pressed {}", command.code)));
        pending.push_receiver(rx, |result| result, input.take_responder());
    }

    // --- /click_at ---

    /// Click at a window position, in logical pixels from the top left, as the mouse would: the
    /// HUD gets it where it is opaque, the world elsewhere. Moves the system cursor there
    #[derive(clap::Parser, ConsoleCommand)]
    #[command(name = "/click_at")]
    pub struct ClickAtCommand {
        x: f32,
        y: f32,
        /// `left` (default), `right` or `middle`
        #[arg(long)]
        button: Option<String>,
    }

    pub fn click_at_cmd(
        mut input: ConsoleCommand<ClickAtCommand>,
        mut script: ResMut<InputScript>,
        mut pending: ResMut<PendingConsoleResponses>,
    ) {
        let Some(Ok(command)) = input.take() else {
            return;
        };
        let button = match command.button.as_deref() {
            None | Some("left") => MouseButton::Left,
            Some("right") => MouseButton::Right,
            Some("middle") => MouseButton::Middle,
            Some(other) => {
                input.reply_failed(format!(
                    "unknown button `{other}`: use left, right or middle"
                ));
                return;
            }
        };
        let position = Vec2::new(command.x, command.y);
        for step in [
            Step::Cursor(position),
            Step::Button(button, ButtonState::Pressed),
            Step::Button(button, ButtonState::Released),
        ] {
            script.push(step);
        }
        let (reply, rx) = RpcResultSender::channel();
        script.push(Step::Done(
            reply,
            format!("clicked at ({}, {})", command.x, command.y),
        ));
        pending.push_receiver(rx, |result| result, input.take_responder());
    }

    const LETTERS: [KeyCode; 26] = [
        KeyCode::KeyA,
        KeyCode::KeyB,
        KeyCode::KeyC,
        KeyCode::KeyD,
        KeyCode::KeyE,
        KeyCode::KeyF,
        KeyCode::KeyG,
        KeyCode::KeyH,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
        KeyCode::KeyM,
        KeyCode::KeyN,
        KeyCode::KeyO,
        KeyCode::KeyP,
        KeyCode::KeyQ,
        KeyCode::KeyR,
        KeyCode::KeyS,
        KeyCode::KeyT,
        KeyCode::KeyU,
        KeyCode::KeyV,
        KeyCode::KeyW,
        KeyCode::KeyX,
        KeyCode::KeyY,
        KeyCode::KeyZ,
    ];

    const DIGITS: [KeyCode; 10] = [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];

    const FUNCTION_KEYS: [(KeyCode, Key); 12] = [
        (KeyCode::F1, Key::F1),
        (KeyCode::F2, Key::F2),
        (KeyCode::F3, Key::F3),
        (KeyCode::F4, Key::F4),
        (KeyCode::F5, Key::F5),
        (KeyCode::F6, Key::F6),
        (KeyCode::F7, Key::F7),
        (KeyCode::F8, Key::F8),
        (KeyCode::F9, Key::F9),
        (KeyCode::F10, Key::F10),
        (KeyCode::F11, Key::F11),
        (KeyCode::F12, Key::F12),
    ];

    /// A DOM `KeyboardEvent.code` name -> the key code, the logical key an unshifted US layout
    /// gives, and the text it types.
    pub fn key_from_code(code: &str) -> Option<(KeyCode, Key, Option<String>)> {
        let single = |s: &str| {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        if let Some(c) = code.strip_prefix("Key").and_then(single) {
            if c.is_ascii_uppercase() {
                let text = c.to_ascii_lowercase().to_string();
                return Some((
                    LETTERS[(c as u8 - b'A') as usize],
                    Key::Character(text.as_str().into()),
                    Some(text),
                ));
            }
        }
        if let Some(c) = code.strip_prefix("Digit").and_then(single) {
            if c.is_ascii_digit() {
                let text = c.to_string();
                return Some((
                    DIGITS[(c as u8 - b'0') as usize],
                    Key::Character(text.as_str().into()),
                    Some(text),
                ));
            }
        }
        if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<usize>().ok()) {
            let (key_code, key) = FUNCTION_KEYS.get(n.checked_sub(1)?)?.clone();
            return Some((key_code, key, None));
        }
        let character = |text: &'static str| Key::Character(text.into());
        let (key_code, key, text): (KeyCode, Key, Option<&str>) = match code {
            "Space" => (KeyCode::Space, Key::Space, Some(" ")),
            "Enter" => (KeyCode::Enter, Key::Enter, Some("\r")),
            "Tab" => (KeyCode::Tab, Key::Tab, Some("\t")),
            "Escape" => (KeyCode::Escape, Key::Escape, None),
            "Backspace" => (KeyCode::Backspace, Key::Backspace, None),
            "Delete" => (KeyCode::Delete, Key::Delete, None),
            "ArrowUp" => (KeyCode::ArrowUp, Key::ArrowUp, None),
            "ArrowDown" => (KeyCode::ArrowDown, Key::ArrowDown, None),
            "ArrowLeft" => (KeyCode::ArrowLeft, Key::ArrowLeft, None),
            "ArrowRight" => (KeyCode::ArrowRight, Key::ArrowRight, None),
            "Home" => (KeyCode::Home, Key::Home, None),
            "End" => (KeyCode::End, Key::End, None),
            "PageUp" => (KeyCode::PageUp, Key::PageUp, None),
            "PageDown" => (KeyCode::PageDown, Key::PageDown, None),
            "ShiftLeft" => (KeyCode::ShiftLeft, Key::Shift, None),
            "ShiftRight" => (KeyCode::ShiftRight, Key::Shift, None),
            "ControlLeft" => (KeyCode::ControlLeft, Key::Control, None),
            "ControlRight" => (KeyCode::ControlRight, Key::Control, None),
            "AltLeft" => (KeyCode::AltLeft, Key::Alt, None),
            "AltRight" => (KeyCode::AltRight, Key::Alt, None),
            "Minus" => (KeyCode::Minus, character("-"), Some("-")),
            "Equal" => (KeyCode::Equal, character("="), Some("=")),
            "Comma" => (KeyCode::Comma, character(","), Some(",")),
            "Period" => (KeyCode::Period, character("."), Some(".")),
            "Slash" => (KeyCode::Slash, character("/"), Some("/")),
            "Semicolon" => (KeyCode::Semicolon, character(";"), Some(";")),
            "Quote" => (KeyCode::Quote, character("'"), Some("'")),
            "Backquote" => (KeyCode::Backquote, character("`"), Some("`")),
            "BracketLeft" => (KeyCode::BracketLeft, character("["), Some("[")),
            "BracketRight" => (KeyCode::BracketRight, character("]"), Some("]")),
            "Backslash" => (KeyCode::Backslash, character("\\"), Some("\\")),
            _ => return None,
        };
        Some((key_code, key, text.map(String::from)))
    }
}

#[cfg(feature = "remote")]
pub mod remote {
    //! A loopback Bevy Remote Protocol server that runs a console command from outside the
    //! process, for the native clients in end-to-end tests and for agents: `dcl/console`
    //! (`{"command": "/teleport 10 20"}`) answers with a call id, and `dcl/console_result`
    //! (`{"id": 1}`) with `{"done": false}` until the command has answered, then with `{"done":
    //! true, "ok": ..., "output": ...}`. Commands answer asynchronously (a login, a walk), and a
    //! Remote Protocol method answers within the frame, hence the two calls.
    //!
    //! Any page open on the machine can post to loopback, so both methods need the run's token
    //! (`"token"`, the one the server was started with: `--automation-token`, else
    //! `DCL_AUTOMATION_TOKEN`). The server exists only with a token. They are the server's only
    //! methods: Bevy's own (`bevy/query`, `bevy/insert`, `bevy/destroy`, ...) check no token.

    use std::collections::BTreeMap;

    use bevy::prelude::*;
    use bevy::remote::{
        error_codes, http::RemoteHttpPlugin, BrpError, BrpResult, RemoteMethods, RemotePlugin,
    };
    use common::rpc::{RpcResultReceiver, RpcResultSender};
    use serde_json::{json, Value};
    use system_bridge::{SystemApi, SystemBridge};

    /// Uncollected results kept; past it the oldest calls are dropped.
    const MAX_PENDING: usize = 256;

    /// The run's token, and the commands sent over the Remote Protocol by call id until their
    /// result is collected.
    #[derive(Resource)]
    pub struct ConsoleCalls {
        token: String,
        last_id: u64,
        pending: BTreeMap<u64, RpcResultReceiver<Result<String, String>>>,
    }

    impl ConsoleCalls {
        fn new(token: String) -> Self {
            Self {
                token,
                last_id: 0,
                pending: BTreeMap::new(),
            }
        }
    }

    const METHODS: [&str; 2] = ["dcl/console", "dcl/console_result"];

    /// The Remote Protocol server: the console's two methods, alone, on loopback at `port`
    /// (`AutomationOptions::server` builds it from the launch options).
    pub struct ConsoleServerPlugin {
        pub port: u16,
        /// Not empty.
        pub token: String,
    }

    impl Plugin for ConsoleServerPlugin {
        fn build(&self, app: &mut App) {
            app.insert_resource(ConsoleCalls::new(self.token.clone()));
            // `RemotePlugin` has no public empty constructor: it registers Bevy's methods with
            // these, and the method table is then cut down to these. Loopback only, as
            // `RemoteHttpPlugin` binds by default
            app.add_plugins((
                RemotePlugin::default()
                    .with_method(METHODS[0], console_send)
                    .with_method(METHODS[1], console_result),
                RemoteHttpPlugin::default().with_port(self.port),
            ));
            let all = app.world().resource::<RemoteMethods>();
            let mut kept = RemoteMethods::new();
            for name in METHODS {
                if let Some(&id) = all.get(name) {
                    kept.insert(name, id);
                }
            }
            app.insert_resource(kept);
        }
    }

    /// `/name arg arg` -> (`/name`, args), split on whitespace as the page's console RPC splits it.
    pub(super) fn split_command_line(line: &str) -> Option<(String, Vec<String>)> {
        let mut parts = line.split_whitespace();
        let cmd = parts.next()?;
        let cmd = if cmd.starts_with('/') {
            cmd.to_owned()
        } else {
            format!("/{cmd}")
        };
        Some((cmd, parts.map(String::from).collect()))
    }

    fn invalid_params(message: &str) -> BrpError {
        BrpError {
            code: error_codes::INVALID_PARAMS,
            message: message.to_owned(),
            data: None,
        }
    }

    /// A call is let through only with the run's token.
    pub(super) fn authorize(expected: &str, given: Option<&str>) -> Result<(), String> {
        match given {
            Some(given) if given == expected => Ok(()),
            _ => Err("missing or wrong \"token\"".to_owned()),
        }
    }

    fn check_token(calls: &ConsoleCalls, params: &Option<Value>) -> Result<(), BrpError> {
        let given = params
            .as_ref()
            .and_then(|p| p.get("token"))
            .and_then(Value::as_str);
        authorize(&calls.token, given).map_err(|message| invalid_params(&message))
    }

    fn console_send(
        In(params): In<Option<Value>>,
        bridge: Res<SystemBridge>,
        mut calls: ResMut<ConsoleCalls>,
    ) -> BrpResult {
        check_token(&calls, &params)?;
        let line = params
            .as_ref()
            .and_then(|p| p.get("command"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_params("expected {\"command\": \"/name args\"}"))?;
        let (cmd, args) =
            split_command_line(line).ok_or_else(|| invalid_params("empty command"))?;
        let (sx, rx) = RpcResultSender::channel();
        bridge
            .sender
            .send(SystemApi::ConsoleCommand(cmd, args, sx))
            .map_err(|_| BrpError {
                code: error_codes::INTERNAL_ERROR,
                message: "engine channel closed".to_owned(),
                data: None,
            })?;
        calls.last_id += 1;
        let id = calls.last_id;
        calls.pending.insert(id, rx);
        while calls.pending.len() > MAX_PENDING {
            calls.pending.pop_first();
        }
        Ok(json!({ "id": id }))
    }

    fn console_result(In(params): In<Option<Value>>, mut calls: ResMut<ConsoleCalls>) -> BrpResult {
        check_token(&calls, &params)?;
        let id = params
            .as_ref()
            .and_then(|p| p.get("id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_params("expected {\"id\": <call id>}"))?;
        let Some(rx) = calls.pending.get_mut(&id) else {
            return Err(invalid_params("unknown or already collected call id"));
        };
        let result = match rx.poll_once() {
            Ok(None) => return Ok(json!({ "done": false })),
            Ok(Some(result)) => result,
            Err(()) => Err("command response dropped".to_owned()),
        };
        calls.pending.remove(&id);
        let (ok, output) = match result {
            Ok(output) => (true, output),
            Err(e) => (false, e),
        };
        Ok(json!({ "done": true, "ok": ok, "output": output }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_codes_name_keys_as_the_dom_does() {
        use bevy::input::keyboard::Key;
        let (code, key, text) = input::key_from_code("KeyB").unwrap();
        assert_eq!(code, KeyCode::KeyB);
        assert_eq!(key, Key::Character("b".into()));
        assert_eq!(text.as_deref(), Some("b"));
        assert_eq!(input::key_from_code("Digit7").unwrap().0, KeyCode::Digit7);
        assert_eq!(input::key_from_code("F12").unwrap().1, Key::F12);
        assert_eq!(
            input::key_from_code("Enter").unwrap().2.as_deref(),
            Some("\r")
        );
        assert_eq!(input::key_from_code("Escape").unwrap().2, None);
        for unknown in ["Keyb", "KeyAB", "Digit10", "F0", "F13", "Hyper", ""] {
            assert!(input::key_from_code(unknown).is_none(), "{unknown}");
        }
    }

    #[test]
    fn typing_presses_and_releases_each_character_then_answers() {
        let mut script = input::InputScript::default();
        let (reply, _rx) = common::rpc::RpcResultSender::channel();
        input::queue_typing(&mut script, "ab\n", reply, "typed".to_owned());
        assert_eq!(script.steps(), 3 * 2 + 1);
    }

    #[test]
    fn the_help_lists_the_options_under_their_own_heading() {
        use clap::CommandFactory;
        let mut command = crate::DecentralandArguments::command();
        // flattening the options' doc comment must not replace the program's
        assert_eq!(
            command.get_about().map(ToString::to_string).as_deref(),
            Some("Decentraland Bevy Explorer")
        );
        let help = command.render_help().to_string();
        let section = help
            .split("Automation:")
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .expect("no Automation heading");
        assert!(section.contains("--automation-token <token>"), "{section}");
        assert!(section.contains("--automation-port <port>"), "{section}");
    }

    /// The tests below need the `remote` feature, which `default` and `headless` both turn on.
    #[cfg(feature = "remote")]
    mod server {
        use bevy::remote::http::DEFAULT_PORT;
        use clap::Parser;

        use super::*;
        use crate::DecentralandArguments;

        #[test]
        fn a_command_line_splits_like_the_pages_console_rpc() {
            assert_eq!(
                remote::split_command_line("  teleport 10   20 "),
                Some((
                    "/teleport".to_owned(),
                    vec!["10".to_owned(), "20".to_owned()]
                ))
            );
            assert_eq!(
                remote::split_command_line("/world_state"),
                Some(("/world_state".to_owned(), vec![]))
            );
            assert_eq!(remote::split_command_line("   "), None);
        }

        #[test]
        fn the_remote_console_needs_the_runs_token() {
            assert!(remote::authorize("s3cret", Some("s3cret")).is_ok());
            assert!(remote::authorize("s3cret", Some("guess")).is_err());
            assert!(remote::authorize("s3cret", None).is_err());
        }

        #[test]
        fn the_remote_server_has_only_the_consoles_methods() {
            let mut app = App::new();
            app.add_plugins(remote::ConsoleServerPlugin {
                port: DEFAULT_PORT,
                token: "s3cret".to_owned(),
            });
            let mut methods = app
                .world()
                .resource::<bevy::remote::RemoteMethods>()
                .methods();
            methods.sort();
            assert_eq!(methods, ["dcl/console", "dcl/console_result"]);
        }

        fn options(args: &[&str]) -> AutomationOptions {
            let args = ["decentra-bevy"].into_iter().chain(args.iter().copied());
            DecentralandArguments::try_parse_from(args)
                .unwrap()
                .automation
        }

        fn started(
            options: &AutomationOptions,
            env_token: Option<&str>,
            env_port: Option<&str>,
        ) -> Option<(u16, String)> {
            options
                .server_from(env_token.map(String::from), env_port.map(String::from))
                .map(|server| (server.port, server.token))
        }

        #[test]
        fn the_server_starts_only_with_a_token() {
            let none = options(&[]);
            assert_eq!(none, AutomationOptions::default());
            assert_eq!(started(&none, None, None), None);
            // a port alone starts nothing, and neither does an empty token
            assert_eq!(
                started(&options(&["--automation-port", "1"]), None, None),
                None
            );
            assert_eq!(
                started(&options(&["--automation-token", ""]), None, None),
                None
            );
            assert_eq!(started(&none, Some(""), None), None);

            let flagged = options(&["--automation-token", "s3cret"]);
            assert_eq!(
                started(&flagged, None, None),
                Some((DEFAULT_PORT, "s3cret".to_owned()))
            );
        }

        #[test]
        fn the_flags_win_over_the_environment() {
            let flagged = options(&["--automation-token", "flag", "--automation-port", "4000"]);
            assert_eq!(
                started(&flagged, Some("env"), Some("5000")),
                Some((4000, "flag".to_owned()))
            );

            let bare = options(&[]);
            assert_eq!(
                started(&bare, Some("env"), Some("5000")),
                Some((5000, "env".to_owned()))
            );
            // the token from the environment, the port from the flag
            let port = options(&["--automation-port", "4000"]);
            assert_eq!(
                started(&port, Some("env"), Some("5000")),
                Some((4000, "env".to_owned()))
            );
        }

        #[test]
        fn a_port_that_is_not_one_falls_back_to_the_default() {
            let flagged = options(&["--automation-token", "s3cret"]);
            for port in ["", "http", "70000", "-1"] {
                assert_eq!(
                    started(&flagged, None, Some(port)),
                    Some((DEFAULT_PORT, "s3cret".to_owned())),
                    "{port}"
                );
            }
            // on the command line it is a usage error instead
            assert!(DecentralandArguments::try_parse_from([
                "decentra-bevy",
                "--automation-port",
                "70000"
            ])
            .is_err());
        }
    }
}
