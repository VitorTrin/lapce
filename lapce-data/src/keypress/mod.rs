#![allow(clippy::module_inception)]

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use druid::piet::{PietTextLayout, Text, TextLayout, TextLayoutBuilder};
use druid::{Command, KbKey};
use druid::{
    Env, EventCtx, ExtEventSink, FontFamily, KeyEvent, Modifiers, PaintCtx, Point,
    Rect, RenderContext, Size, Target,
};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use indexmap::IndexMap;
use itertools::Itertools;
use toml;

mod keypress;
mod loader;

use crate::command::{
    lapce_internal_commands, CommandExecuted, CommandTarget, LapceCommandNew,
    LapceUICommand, LAPCE_NEW_COMMAND, LAPCE_UI_COMMAND,
};
use crate::config::{Config, LapceTheme};
use crate::keypress::loader::KeyMapLoader;
use crate::state::Modes;
use crate::{command::LapceCommand, state::Mode};

pub use keypress::KeyPress;

const DEFAULT_KEYMAPS_COMMON: &str =
    include_str!("../../../defaults/keymaps-common.toml");
const DEFAULT_KEYMAPS_MACOS: &str =
    include_str!("../../../defaults/keymaps-macos.toml");
const DEFAULT_KEYMAPS_NONMACOS: &str =
    include_str!("../../../defaults/keymaps-nonmacos.toml");

#[derive(PartialEq)]
enum KeymapMatch {
    Full(String),
    Multiple(Vec<String>),
    Prefix,
    None,
}

pub fn paint_key(
    ctx: &mut PaintCtx,
    text: &str,
    origin: Point,
    config: &Config,
) -> (Rect, PietTextLayout, Point) {
    let text_layout = ctx
        .text()
        .new_text_layout(text.to_string())
        .font(FontFamily::SYSTEM_UI, 13.0)
        .text_color(
            config
                .get_color_unchecked(LapceTheme::EDITOR_FOREGROUND)
                .clone(),
        )
        .build()
        .unwrap();
    let text_size = text_layout.size();
    let text_layout_point = origin + (5.0, -(text_size.height / 2.0));
    let rect = Size::new(text_size.width, 0.0)
        .to_rect()
        .with_origin(origin + (5.0, 0.0))
        .inflate(5.0, text_size.height / 2.0 + 4.0);
    (rect, text_layout, text_layout_point)
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct KeyMap {
    pub key: Vec<KeyPress>,
    pub modes: Modes,
    pub when: Option<String>,
    pub command: String,
}

pub enum Alignment {
    Left,
    Center,
    Right,
}

impl KeyMap {
    pub fn paint(
        &self,
        ctx: &mut PaintCtx,
        origin: Point,
        align: Alignment,
        config: &Config,
    ) {
        let old_origin = origin;

        let mut origin = origin;
        let mut items = Vec::new();
        for keypress in self.key.iter() {
            let (new_origin, mut new_items) = keypress.paint(ctx, origin, config);
            origin = new_origin + (10.0, 0.0);
            items.append(&mut new_items);
        }

        let x_shift = match align {
            Alignment::Left => 0.0,
            Alignment::Center => (origin.x - old_origin.x) / 2.0,
            Alignment::Right => (origin.x - old_origin.x),
        };

        for (rect, text_layout, text_layout_pos) in items {
            if let Some(mut rect) = rect {
                rect.x0 -= x_shift;
                rect.x1 -= x_shift;
                ctx.stroke(
                    rect,
                    config.get_color_unchecked(LapceTheme::LAPCE_BORDER),
                    1.0,
                );
            }
            ctx.draw_text(&text_layout, text_layout_pos - (x_shift, 0.0));
        }
    }
}

pub trait KeyPressFocus {
    fn get_mode(&self) -> Mode;
    fn check_condition(&self, condition: &str) -> bool;
    fn run_command(
        &mut self,
        ctx: &mut EventCtx,
        command: &LapceCommand,
        count: Option<usize>,
        mods: Modifiers,
        env: &Env,
    ) -> CommandExecuted;
    fn expect_char(&self) -> bool {
        false
    }
    fn receive_char(&mut self, ctx: &mut EventCtx, c: &str);
}

#[derive(Clone)]
pub struct KeyPressData {
    pending_keypress: Vec<KeyPress>,
    pub commands: Arc<IndexMap<String, LapceCommandNew>>,
    pub keymaps: Arc<IndexMap<Vec<KeyPress>, Vec<KeyMap>>>,
    pub command_keymaps: Arc<IndexMap<String, Vec<KeyMap>>>,

    pub commands_with_keymap: Arc<Vec<KeyMap>>,
    pub commands_without_keymap: Arc<Vec<LapceCommandNew>>,
    pub filtered_commands_with_keymap: Arc<Vec<KeyMap>>,
    pub filtered_commands_without_keymap: Arc<Vec<LapceCommandNew>>,
    pub filter_pattern: String,

    count: Option<usize>,

    event_sink: ExtEventSink,
}

impl KeyPressData {
    pub fn new(config: &Config, event_sink: ExtEventSink) -> Self {
        let (keymaps, command_keymaps) =
            Self::get_keymaps(config).unwrap_or((IndexMap::new(), IndexMap::new()));
        let mut keypress = Self {
            pending_keypress: Vec::new(),
            commands: Arc::new(lapce_internal_commands()),
            keymaps: Arc::new(keymaps),
            command_keymaps: Arc::new(command_keymaps),
            commands_with_keymap: Arc::new(Vec::new()),
            commands_without_keymap: Arc::new(Vec::new()),
            filter_pattern: "".to_string(),
            filtered_commands_with_keymap: Arc::new(Vec::new()),
            filtered_commands_without_keymap: Arc::new(Vec::new()),
            count: None,
            event_sink,
        };
        keypress.load_commands();
        keypress
    }

    pub fn update_keymaps(&mut self, config: &Config) {
        if let Ok((new_keymaps, new_command_keymaps)) = Self::get_keymaps(config) {
            self.keymaps = Arc::new(new_keymaps);
            self.command_keymaps = Arc::new(new_command_keymaps);
            self.load_commands();
        }
    }

    fn load_commands(&mut self) {
        let mut commands_with_keymap = Vec::new();
        let mut commands_without_keymap = Vec::new();
        for (_, keymaps) in self.command_keymaps.iter() {
            for keymap in keymaps.iter() {
                if self.commands.get(&keymap.command).is_some() {
                    commands_with_keymap.push(keymap.clone());
                }
            }
        }

        for (_, cmd) in self.commands.iter() {
            if !self.command_keymaps.contains_key(&cmd.cmd) {
                commands_without_keymap.push(cmd.clone());
            }
        }

        self.commands_with_keymap = Arc::new(commands_with_keymap);
        self.commands_without_keymap = Arc::new(commands_without_keymap);
        if !self.filter_pattern.is_empty() {
            self.filter_commands(&self.filter_pattern.clone());
        }
    }

    fn run_command<T: KeyPressFocus>(
        &self,
        ctx: &mut EventCtx,
        command: &str,
        count: Option<usize>,
        mods: Modifiers,
        focus: &mut T,
        env: &Env,
    ) -> CommandExecuted {
        if let Some(cmd) = self.commands.get(command) {
            if let CommandTarget::Focus = cmd.target {
                if let Ok(cmd) = LapceCommand::from_str(command) {
                    focus.run_command(ctx, &cmd, count, mods, env)
                } else {
                    CommandExecuted::No
                }
            } else {
                ctx.submit_command(Command::new(
                    LAPCE_NEW_COMMAND,
                    cmd.clone(),
                    Target::Auto,
                ));
                CommandExecuted::Yes
            }
        } else {
            CommandExecuted::No
        }
    }

    fn handle_count<T: KeyPressFocus>(
        &mut self,
        focus: &T,
        keypress: &KeyPress,
    ) -> bool {
        if focus.expect_char() {
            return false;
        }
        let mode = focus.get_mode();
        if mode == Mode::Insert || mode == Mode::Terminal {
            return false;
        }

        if !keypress.mods.is_empty() {
            return false;
        }

        if let druid::KbKey::Character(c) = &keypress.key {
            if let Ok(n) = c.parse::<usize>() {
                if self.count.is_some() || n > 0 {
                    self.count = Some(self.count.unwrap_or(0) * 10 + n);
                    return true;
                }
            }
        }

        false
    }

    fn get_key_modifiers(key_event: &KeyEvent) -> Modifiers {
        let mut mods = key_event.mods;

        if matches!(key_event.key, KbKey::Shift | KbKey::Character(_)) {
            mods.set(Modifiers::SHIFT, false);
        }

        mods
    }

    pub fn keypress(key_event: &KeyEvent) -> Option<KeyPress> {
        match key_event.key {
            KbKey::Shift
            | KbKey::Meta
            | KbKey::Super
            | KbKey::Alt
            | KbKey::Control => None,
            ref key => Some(KeyPress {
                key: key.clone(),
                mods: Self::get_key_modifiers(key_event),
            }),
        }
    }

    pub fn key_down<T: KeyPressFocus>(
        &mut self,
        ctx: &mut EventCtx,
        key_event: &KeyEvent,
        focus: &mut T,
        env: &Env,
    ) -> bool {
        log::info!("Keypress: {key_event:?}");

        // We are removing Shift modifier since the character is already upper case.
        let mods = Self::get_key_modifiers(key_event);

        if key_event.key == KbKey::Shift && mods.is_empty() {
            return false;
        }

        let keypress = KeyPress {
            key: key_event.key.clone(),
            mods,
        };

        let mode = focus.get_mode();
        if self.handle_count(focus, &keypress) {
            return false;
        }

        self.pending_keypress.push(keypress.clone());

        let keymatch = self.match_keymap(&self.pending_keypress, focus);
        match keymatch {
            KeymapMatch::Full(command) => {
                self.pending_keypress.clear();
                let count = self.count.take();
                self.run_command(ctx, &command, count, mods, focus, env);
                return true;
            }
            KeymapMatch::Multiple(commands) => {
                self.pending_keypress.clear();
                let count = self.count.take();
                for command in commands {
                    if self.run_command(ctx, &command, count, mods, focus, env)
                        == CommandExecuted::Yes
                    {
                        return true;
                    }
                }

                return true;
            }
            KeymapMatch::Prefix => {
                // Here pending_keypress contains only a prefix of some keymap, so let's keep
                // collecting key presses.
                return false;
            }
            KeymapMatch::None => {
                self.pending_keypress.clear();
                if focus.get_mode() == Mode::Insert {
                    let mut keypress = keypress.clone();
                    keypress.mods.set(Modifiers::SHIFT, false);
                    if let KeymapMatch::Full(command) =
                        self.match_keymap(&[keypress], focus)
                    {
                        if let Ok(cmd) = LapceCommand::from_str(&command) {
                            if cmd.move_command(None).is_some() {
                                focus.run_command(ctx, &cmd, None, mods, env);
                                return true;
                            }
                        }
                    }
                }
            }
        }

        if mode != Mode::Insert
            && mode != Mode::Terminal
            && self.handle_count(focus, &keypress)
        {
            return false;
        }

        self.count = None;

        if keypress.mods.is_empty() {
            if let druid::KbKey::Character(c) = &key_event.key {
                focus.receive_char(ctx, c);
                return true;
            }
        }
        false
    }

    fn match_keymap<T: KeyPressFocus>(
        &self,
        keypresses: &[KeyPress],
        check: &T,
    ) -> KeymapMatch {
        let matches = self
            .keymaps
            .get(keypresses)
            .map(|keymaps| {
                keymaps
                    .iter()
                    .filter(|keymap| {
                        if check.expect_char()
                            && keypresses.len() == 1
                            && keypresses[0].is_char()
                        {
                            return false;
                        }
                        if !keymap.modes.is_empty()
                            && !keymap.modes.contains(check.get_mode().into())
                        {
                            return false;
                        }
                        if let Some(condition) = &keymap.when {
                            if !Self::check_condition(condition, check) {
                                return false;
                            }
                        }
                        true
                    })
                    .collect()
            })
            .unwrap_or_else(Vec::new);

        if matches.is_empty() {
            KeymapMatch::None
        } else if matches.len() == 1 && matches[0].key == keypresses {
            KeymapMatch::Full(matches[0].command.clone())
        } else if matches.len() > 1
            && matches.iter().filter(|m| m.key != keypresses).count() == 0
        {
            KeymapMatch::Multiple(
                matches.iter().rev().map(|m| m.command.clone()).collect(),
            )
        } else {
            KeymapMatch::Prefix
        }
    }

    fn check_condition<T: KeyPressFocus>(condition: &str, check: &T) -> bool {
        fn check_one_condition<T: KeyPressFocus>(
            condition: &str,
            check: &T,
        ) -> bool {
            let trimmed = condition.trim();
            if let Some(stripped) = trimmed.strip_prefix('!') {
                !check.check_condition(stripped)
            } else {
                check.check_condition(trimmed)
            }
        }

        match Condition::parse_first(condition) {
            Condition::Single(condition) => check_one_condition(condition, check),
            Condition::Or(left, right) => {
                let left = check_one_condition(left, check);
                let right = Self::check_condition(right, check);

                left || right
            }
            Condition::And(left, right) => {
                let left = check_one_condition(left, check);
                let right = Self::check_condition(right, check);

                left && right
            }
        }
    }

    fn get_file_array() -> Option<toml::value::Array> {
        let path = Self::file()?;
        let content = std::fs::read(&path).ok()?;
        let toml_value: toml::Value = toml::from_slice(&content).ok()?;
        let table = toml_value.as_table()?;
        table.get("keymaps")?.as_array().cloned()
    }

    pub fn filter_commands(&mut self, pattern: &str) {
        self.filter_pattern = pattern.to_string();
        let pattern = pattern.to_string();
        let commands_with_keymap = self.commands_with_keymap.clone();
        let commands_without_keymap = self.commands_without_keymap.clone();
        let commands = self.commands.clone();
        let event_sink = self.event_sink.clone();

        std::thread::spawn(move || {
            let matcher = SkimMatcherV2::default().ignore_case();

            let filtered_commands_with_keymap: Vec<KeyMap> = commands_with_keymap
                .iter()
                .filter_map(|i| {
                    let cmd = commands.get(&i.command).unwrap();
                    let text = cmd
                        .palette_desc
                        .as_ref()
                        .map(|s| s.as_str())
                        .unwrap_or_else(|| cmd.cmd.as_str());

                    matcher.fuzzy_match(text, &pattern).map(|score| (i, score))
                })
                .sorted_by_key(|(_i, score)| -*score)
                .map(|(i, _)| i.clone())
                .collect();

            let filtered_commands_without_keymap: Vec<LapceCommandNew> =
                commands_without_keymap
                    .iter()
                    .filter_map(|i| {
                        let text = i
                            .palette_desc
                            .as_ref()
                            .map(|s| s.as_str())
                            .unwrap_or_else(|| i.cmd.as_str());

                        matcher.fuzzy_match(text, &pattern).map(|score| (i, score))
                    })
                    .sorted_by_key(|(_i, score)| -*score)
                    .map(|(i, _)| i.clone())
                    .collect();

            let _ = event_sink.submit_command(
                LAPCE_UI_COMMAND,
                LapceUICommand::FilterKeymaps(
                    pattern,
                    Arc::new(filtered_commands_with_keymap),
                    Arc::new(filtered_commands_without_keymap),
                ),
                Target::Auto,
            );
        });
    }

    pub fn update_file(keymap: &KeyMap, keys: &[KeyPress]) -> Option<()> {
        let mut array = Self::get_file_array().unwrap_or_default();
        if let Some(index) = array.iter().position(|value| {
            Some(keymap.command.as_str())
                == value.get("command").and_then(|c| c.as_str())
                && keymap.when.as_deref()
                    == value.get("when").and_then(|w| w.as_str())
                && keymap.modes == get_modes(value)
                && Some(keymap.key.clone())
                    == value
                        .get("key")
                        .and_then(|v| v.as_str())
                        .map(KeyPress::parse)
        }) {
            if !keys.is_empty() {
                array[index].as_table_mut()?.insert(
                    "key".to_string(),
                    toml::Value::String(
                        keys.iter().map(|k| k.to_string()).join(" "),
                    ),
                );
            } else {
                array.remove(index);
            };
        } else {
            let mut table = toml::value::Table::new();
            table.insert(
                "command".to_string(),
                toml::Value::String(keymap.command.clone()),
            );
            if !keymap.modes.is_empty() {
                table.insert(
                    "mode".to_string(),
                    toml::Value::String(keymap.modes.to_string()),
                );
            }
            if let Some(when) = keymap.when.as_ref() {
                table.insert(
                    "when".to_string(),
                    toml::Value::String(when.to_string()),
                );
            }

            if !keys.is_empty() {
                table.insert(
                    "key".to_string(),
                    toml::Value::String(
                        keys.iter().map(|k| k.to_string()).join(" "),
                    ),
                );
                array.push(toml::Value::Table(table.clone()));
            }

            if !keymap.key.is_empty() {
                table.insert(
                    "key".to_string(),
                    toml::Value::String(
                        keymap.key.iter().map(|k| k.to_string()).join(" "),
                    ),
                );
                table.insert(
                    "command".to_string(),
                    toml::Value::String(format!("-{}", keymap.command)),
                );
                array.push(toml::Value::Table(table.clone()));
            }
        }

        let mut table = toml::value::Table::new();
        table.insert("keymaps".to_string(), toml::Value::Array(array));
        let value = toml::Value::Table(table);

        let path = Self::file()?;
        std::fs::write(&path, toml::to_string(&value).ok()?.as_bytes()).ok()?;
        None
    }

    pub fn file() -> Option<PathBuf> {
        let path = Config::dir().map(|d| {
            d.join(if !cfg!(debug_assertions) {
                "keymaps.toml"
            } else {
                "debug-keymaps.toml"
            })
        })?;

        if let Some(dir) = path.parent() {
            if !dir.exists() {
                let _ = std::fs::create_dir_all(dir);
            }
        }

        if !path.exists() {
            let _ = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path);
        }

        Some(path)
    }

    #[allow(clippy::type_complexity)]
    fn get_keymaps(
        config: &Config,
    ) -> Result<(
        IndexMap<Vec<KeyPress>, Vec<KeyMap>>,
        IndexMap<String, Vec<KeyMap>>,
    )> {
        let is_modal = config.lapce.modal;

        let mut loader = KeyMapLoader::new();

        if let Err(err) = loader.load_from_str(DEFAULT_KEYMAPS_COMMON, is_modal) {
            log::error!("Failed to load common defaults: {err}");
        }

        let os_keymaps = if std::env::consts::OS == "macos" {
            DEFAULT_KEYMAPS_MACOS
        } else {
            DEFAULT_KEYMAPS_NONMACOS
        };

        if let Err(err) = loader.load_from_str(os_keymaps, is_modal) {
            log::error!("Failed to load OS defaults: {err}");
        }

        if let Some(path) = Self::file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Err(err) = loader.load_from_str(&content, is_modal) {
                    log::error!("Failed to load from {path:?}: {err}");
                }
            }
        }

        Ok(loader.finalize())
    }
}

pub struct DefaultKeyPressHandler {}

impl KeyPressFocus for DefaultKeyPressHandler {
    fn get_mode(&self) -> Mode {
        Mode::Normal
    }

    fn check_condition(&self, _condition: &str) -> bool {
        false
    }

    fn run_command(
        &mut self,
        _ctx: &mut EventCtx,
        _command: &LapceCommand,
        _count: Option<usize>,
        _mods: Modifiers,
        _env: &Env,
    ) -> CommandExecuted {
        CommandExecuted::Yes
    }

    fn receive_char(&mut self, _ctx: &mut EventCtx, _c: &str) {}
}

fn get_modes(toml_keymap: &toml::Value) -> Modes {
    toml_keymap
        .get("mode")
        .and_then(|v| v.as_str())
        .map(Modes::parse)
        .unwrap_or_else(Modes::empty)
}

#[derive(Debug, PartialEq, Eq)]
enum Condition<'a> {
    Single(&'a str),
    Or(&'a str, &'a str),
    And(&'a str, &'a str),
}

impl<'a> Condition<'a> {
    fn parse_first(condition: &'a str) -> Self {
        let or = condition.match_indices("||").next();
        let and = condition.match_indices("&&").next();

        match (or, and) {
            (None, None) => Condition::Single(condition),
            (Some((pos, _)), None) => {
                Condition::Or(&condition[..pos], &condition[pos + 2..])
            }
            (None, Some((pos, _))) => {
                Condition::And(&condition[..pos], &condition[pos + 2..])
            }
            (Some((or_pos, _)), Some((and_pos, _))) => {
                if or_pos < and_pos {
                    Condition::Or(&condition[..or_pos], &condition[or_pos + 2..])
                } else {
                    Condition::And(&condition[..and_pos], &condition[and_pos + 2..])
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use crate::keypress::{Condition, KeyPressData, KeyPressFocus};

    struct MockFocus {
        accepted_conditions: &'static [&'static str],
    }

    impl KeyPressFocus for MockFocus {
        fn check_condition(&self, condition: &str) -> bool {
            self.accepted_conditions.contains(&condition)
        }

        fn get_mode(&self) -> crate::state::Mode {
            unimplemented!()
        }

        fn run_command(
            &mut self,
            _ctx: &mut druid::EventCtx,
            _command: &crate::command::LapceCommand,
            _count: Option<usize>,
            _mods: druid::Modifiers,
            _env: &druid::Env,
        ) -> crate::command::CommandExecuted {
            unimplemented!()
        }

        fn receive_char(&mut self, _ctx: &mut druid::EventCtx, _c: &str) {
            unimplemented!()
        }
    }

    #[test]
    fn test_parse() {
        assert_eq!(
            Condition::Or("foo", "bar"),
            Condition::parse_first("foo||bar")
        );
        assert_eq!(
            Condition::And("foo", "bar"),
            Condition::parse_first("foo&&bar")
        );
        assert_eq!(
            Condition::And("foo", "bar||baz"),
            Condition::parse_first("foo&&bar||baz")
        );
        assert_eq!(
            Condition::And("foo ", " bar || baz"),
            Condition::parse_first("foo && bar || baz")
        );
    }

    #[test]
    fn test_check_condition() {
        let focus = MockFocus {
            accepted_conditions: &["foo", "bar"],
        };

        let test_cases = [
            ("foo", true),
            ("bar", true),
            ("!foo", false),
            ("!bar", false),
            ("foo || bar", true),
            ("foo || !bar", true),
            ("!foo || bar", true),
            ("foo && bar", true),
            ("foo && !bar", false),
            ("!foo && bar", false),
            ("foo && bar || baz", true),
            ("foo && bar && baz", false),
            ("foo && bar && !baz", true),
        ];

        for (condition, should_accept) in test_cases.into_iter() {
            assert_eq!(
                should_accept,
                KeyPressData::check_condition(condition, &focus),
                "Condition check failed. Condition: {condition}. Expected result: {should_accept}",
            );
        }
    }
}
