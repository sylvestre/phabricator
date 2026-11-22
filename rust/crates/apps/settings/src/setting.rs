//! The settings of the panels Rust shows (`PhabricatorSetting` and its
//! subclasses): their options, defaults, validation and the values the
//! editor stores.

use phab_core::{Config, php};
use serde_json::{Value, json};

use crate::zones;

/// What the options of some settings depend on.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The configuration.
    pub config: &'a Config,
    /// The username of the viewer, which the pronoun options show.
    pub viewer_name: &'a str,
    /// The current time, which time zone offsets depend on.
    pub now: i64,
}

/// Options of a select, under a label when they are an `<optgroup>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionGroup {
    /// The `<optgroup>` label; `None` for the options of a plain select.
    pub label: Option<String>,
    /// The values and their labels, in order.
    pub options: Vec<(String, String)>,
}

/// The form control of a setting (`newCustomEditField()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// `PhabricatorSelectEditField`: a select, maybe with option groups.
    Select(Vec<OptionGroup>),
    /// `PhabricatorTextEditField`.
    Text,
}

#[derive(Clone, Copy)]
enum Kind {
    /// `PhabricatorSelectSetting`.
    Select(fn(&Context<'_>) -> Vec<(String, String)>),
    /// `PhabricatorOptionGroupSetting`.
    Groups(fn(&Context<'_>) -> Vec<OptionGroup>),
    /// `PhabricatorStringSetting`, with the validation of the setting.
    Text(fn(&str) -> bool),
    /// `PhabricatorInternalSetting`: no control, values stored as given.
    Internal,
}

/// One setting (`PhabricatorSetting`).
pub struct Setting {
    /// `SETTINGKEY`, also the form field name.
    pub key: &'static str,
    /// `getSettingName()`, the field label.
    pub name: &'static str,
    /// `getSettingPanelKey()`.
    pub panel: Option<&'static str>,
    order: i64,
    /// `getControlInstructions()`, as Remarkup.
    pub instructions: Option<&'static str>,
    kind: Kind,
    default: fn(&Context<'_>) -> Value,
}

impl std::fmt::Debug for Setting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Setting")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// `PhabricatorTimezoneSetting::SETTINGKEY`.
pub const TIMEZONE: &str = "timezone";
/// `PhabricatorTimezoneIgnoreOffsetSetting::SETTINGKEY`.
pub const TIMEZONE_IGNORE_OFFSET: &str = "time.offset.ignore";
/// `PhabricatorTranslationSetting::SETTINGKEY`.
pub const TRANSLATION: &str = "translation";
/// `PhabricatorEmailTagsSetting::SETTINGKEY`.
pub const MAIL_TAGS: &str = "mailtags";

fn pairs(options: &[(&str, &str)]) -> Vec<(String, String)> {
    options
        .iter()
        .map(|(key, label)| ((*key).to_owned(), (*label).to_owned()))
        .collect()
}

/// `date_default_timezone_get()`: `phabricator.timezone` when PHP accepts
/// it, UTC otherwise.
pub fn default_timezone(config: &Config) -> String {
    config
        .nonempty_string("phabricator.timezone")
        .filter(|name| jiff::tz::TimeZone::get(name).is_ok())
        .unwrap_or("UTC")
        .to_owned()
}

/// `PhabricatorTimezoneSetting::getSelectOptionGroups()`: the zones by
/// their offset now, each group sorted.
fn timezone_groups(context: &Context<'_>) -> Vec<OptionGroup> {
    let now = jiff::Timestamp::from_second(context.now).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let mut offsets: Vec<(i64, Vec<&str>)> = Vec::new();
    for identifier in zones::IDENTIFIERS {
        let Ok(zone) = jiff::tz::TimeZone::get(identifier) else {
            continue;
        };
        let minutes = i64::from(zone.to_offset(now).seconds()) / 60;
        match offsets.iter_mut().find(|(offset, _)| *offset == minutes) {
            Some((_, group)) => group.push(identifier),
            None => offsets.push((minutes, vec![identifier])),
        }
    }
    offsets.sort_by_key(|(offset, _)| *offset);
    let mut groups = vec![OptionGroup {
        label: Some("Default".to_owned()),
        options: Vec::new(),
    }];
    for (offset, mut group) in offsets {
        // `%+d` of `$offset / 60` truncates toward zero.
        let hours = offset / 60;
        let minutes = (offset % 60).abs();
        let label = if minutes == 0 {
            format!("UTC{hours:+}")
        } else {
            format!("UTC{hours:+}:{minutes:02}")
        };
        group.sort_unstable();
        groups.push(OptionGroup {
            label: Some(label),
            options: group
                .into_iter()
                .map(|identifier| (identifier.to_owned(), identifier.replace('_', " ")))
                .collect(),
        });
    }
    groups
}

/// `PhabricatorTranslationSetting::getSelectOptionGroups()` outside
/// developer mode: the English locales, the others having too few strings.
fn translation_groups(_: &Context<'_>) -> Vec<OptionGroup> {
    vec![OptionGroup {
        label: Some("Translations".to_owned()),
        options: pairs(&[
            ("en_CA", "English (Canada)"),
            ("en_GB", "English (Great Britain)"),
            ("en_US", "English (US)"),
        ]),
    }]
}

fn pronoun_options(context: &Context<'_>) -> Vec<(String, String)> {
    let name = context.viewer_name;
    vec![
        (String::new(), format!("{name} updated their profile")),
        ("m".to_owned(), format!("{name} updated his profile")),
        ("f".to_owned(), format!("{name} updated her profile")),
    ]
}

/// `PhabricatorMonospacedFontSetting::filterMonospacedCSSRule()` leaves
/// the value as it is.
fn is_safe_font(value: &str) -> bool {
    value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || " ,\"./-".contains(character))
}

const DATE_FORMAT_INSTRUCTIONS: &str = "Select the format you prefer for editing dates.";

const NO_MAIL_INSTRUCTIONS: &str = " - To receive the new Mozilla-specific emails, select \
**Mozilla Notifications**. Note that herald rules will no longer send you emails.
 - To continue to receive regular Phabricator emails, including Herald mail, choose **Legacy \
Phabricator Notifications**.
 - If you select **Disable Email Notifications**, this server will never send email to notify \
you about events. This preference overrides all your other settings.

//Regardless of this setting, you will still receive some administrative email, like password \
reset email.//";

const RE_PREFIX_INSTRUCTIONS: &str = "The **Add \"Re:\" Prefix** setting adds \"Re:\" in front \
of all messages, even if they are not replies. If you use **Mail.app** on Mac OS X, this may \
improve mail threading.

| Setting                | Example Mail Subject
|------------------------|----------------
| Enable \"Re:\" Prefix  | `Re: [Differential] [Accepted] D123: Example Revision`
| Disable \"Re:\" Prefix | `[Differential] [Accepted] D123: Example Revision`";

const VARY_SUBJECT_INSTRUCTIONS: &str = "With **Vary Subjects** enabled, most mail subject lines \
will include a brief description of their content, like `[Closed]` for a notification about \
someone closing a task.

| Setting              | Example Mail Subject
|----------------------|----------------
| Vary Subjects        | `[Maniphest] [Closed] T123: Example Task`
| Do Not Vary Subjects | `[Maniphest] T123: Example Task`

This can make mail more useful, but some clients have difficulty threading these messages. \
Disabling this option may improve threading at the cost of making subject lines less useful.";

const STAMPS_INSTRUCTIONS: &str = "Outgoing mail is stamped with labels like `actor(alice)` \
which can be used to
write client mail rules to organize mail. By default, these stamps are sent
in an `X-Phabricator-Stamps` header.

If you use a client which can not use headers to route mail (like Gmail),
you can also include the stamps in the message body so mail rules based on
body content can route messages.";

/// The settings Rust knows, in no particular order.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: TIMEZONE,
        name: "Timezone",
        panel: Some("datetime"),
        order: 100,
        instructions: Some("Select your local timezone."),
        kind: Kind::Groups(timezone_groups),
        default: |context| json!(default_timezone(context.config)),
    },
    Setting {
        key: "date-format",
        name: "Date Format",
        panel: Some("datetime"),
        order: 200,
        instructions: Some(DATE_FORMAT_INSTRUCTIONS),
        kind: Kind::Select(|_| {
            pairs(&[
                ("Y-m-d", "ISO 8601: 2000-02-28"),
                ("n/j/Y", "US: 2/28/2000"),
                ("d-m-Y", "Europe: 28-02-2000"),
            ])
        }),
        default: |_| json!("Y-m-d"),
    },
    Setting {
        key: "time-format",
        name: "Time Format",
        panel: Some("datetime"),
        order: 300,
        instructions: Some("Select the format you prefer for editing and displaying time."),
        kind: Kind::Select(|_| pairs(&[("g:i A", "12 Hour, 2:34 PM"), ("H:i", "24 Hour, 14:34")])),
        default: |_| json!("g:i A"),
    },
    Setting {
        key: "week-start-day",
        name: "Week Starts On",
        panel: Some("datetime"),
        order: 400,
        instructions: Some("Choose which day a calendar week should begin on."),
        kind: Kind::Select(|_| {
            pairs(&[
                ("0", "Sunday"),
                ("1", "Monday"),
                ("2", "Tuesday"),
                ("3", "Wednesday"),
                ("4", "Thursday"),
                ("5", "Friday"),
                ("6", "Saturday"),
            ])
        }),
        default: |_| json!(0),
    },
    Setting {
        key: TIMEZONE_IGNORE_OFFSET,
        name: "Timezone Ignored Offset",
        panel: None,
        order: 1000,
        instructions: None,
        kind: Kind::Internal,
        default: |_| Value::Null,
    },
    Setting {
        key: TRANSLATION,
        name: "Translation",
        panel: Some("language"),
        order: 100,
        instructions: Some("Choose which language you would like the UI to use."),
        kind: Kind::Groups(translation_groups),
        default: |_| json!("en_US"),
    },
    Setting {
        key: "pronoun",
        name: "Pronoun",
        panel: Some("language"),
        order: 200,
        instructions: Some("Choose the pronoun you prefer."),
        kind: Kind::Select(pronoun_options),
        default: |_| json!(""),
    },
    Setting {
        key: "diff-unified",
        name: "Show Unified Diffs",
        panel: Some("diff"),
        order: 100,
        instructions: Some(
            "Diffs are normally shown in a side-by-side layout on large screens and \
             automatically switched to a unified view on small screens (like mobile phones). \
             If you prefer unified diffs even on large screens, you can select them for use \
             on all displays.",
        ),
        kind: Kind::Select(|_| pairs(&[("default", "On Small Screens"), ("unified", "Always")])),
        default: |_| json!("default"),
    },
    Setting {
        key: "diff-ghosts",
        name: "Show Older Inlines",
        panel: Some("diff"),
        order: 200,
        instructions: Some(
            "When a revision is updated, this software attempts to bring inline comments on \
             the older version forward to the new changes. You can disable this behavior if \
             you prefer comments stay anchored in one place.",
        ),
        kind: Kind::Select(|_| pairs(&[("default", "Enabled"), ("disabled", "Disabled")])),
        default: |_| json!("default"),
    },
    Setting {
        key: "resource-postprocessor",
        name: "Accessibility",
        panel: Some("display"),
        order: 100,
        instructions: Some("If you have difficulty reading the UI, this setting may help."),
        kind: Kind::Select(|_| {
            pairs(&[
                ("default", "Use Standard Colors"),
                ("darkmode", "Dark Mode (Experimental)"),
                ("contrast", "Use High Contrast Colors"),
                ("fontsizeplusone", "Use Larger Font Size"),
                ("redgreen", "Use Red/Green (Deuteranopia) Colors"),
            ])
        }),
        default: |_| json!("default"),
    },
    Setting {
        key: "titles",
        name: "Page Titles",
        panel: Some("display"),
        order: 200,
        instructions: Some(
            "Some applications use unicode glyphs in page titles to provide a compact \
             representation of the current application. You can substitute plain text \
             instead if these glyphs do not display on your system.",
        ),
        kind: Kind::Select(|_| {
            pairs(&[
                ("glyph", "Use Unicode Glyphs: \u{2699}"),
                ("text", "Use Plain Text: [Differential]"),
            ])
        }),
        default: |_| json!("glyph"),
    },
    Setting {
        key: "monospaced",
        name: "Monospaced Font",
        panel: Some("display"),
        order: 500,
        instructions: Some(
            "You can customize the font used when showing monospaced text, including source \
             code. You should enter a valid CSS font declaration like: `13px Consolas`",
        ),
        kind: Kind::Text(is_safe_font),
        default: |_| Value::Null,
    },
    Setting {
        key: "monospaced-textareas",
        name: "Monospaced Textareas",
        panel: Some("display"),
        order: 600,
        instructions: Some(
            "You can choose to use either a monospaced or variable-width font in textareas in \
             the UI. Textareas are used for editing descriptions and writing comments, among \
             other things.",
        ),
        kind: Kind::Select(|_| {
            pairs(&[
                ("disabled", "Use Variable-Width Font"),
                ("enabled", "Use Monospaced Font"),
            ])
        }),
        default: |_| json!("disabled"),
    },
    Setting {
        key: "no-mail",
        name: "Email Notifications",
        panel: Some("emaildelivery"),
        order: 100,
        instructions: Some(NO_MAIL_INSTRUCTIONS),
        kind: Kind::Select(|_| {
            pairs(&[
                ("2", "Mozilla Notifications"),
                ("0", "Legacy Phabricator Notifications"),
                ("1", "Disable Email Notifications"),
            ])
        }),
        default: |context| {
            context
                .config
                .value("email.default")
                .cloned()
                .unwrap_or_else(|| json!("0"))
        },
    },
    Setting {
        key: "self-mail",
        name: "Self Actions",
        panel: Some("emaildelivery"),
        order: 200,
        instructions: Some(
            "If you disable **Self Actions**, Legacy Notifications will not notify you about \
             actions you take.",
        ),
        kind: Kind::Select(|_| {
            pairs(&[
                ("0", "Enable Self Action Mail"),
                ("1", "Disable Self Action Mail"),
            ])
        }),
        default: |_| json!("0"),
    },
    Setting {
        key: "html-emails",
        name: "HTML Email",
        panel: Some("emailformat"),
        order: 100,
        instructions: Some(
            "You can opt to receive plain text email instead of HTML email. Plain text email \
             works better with some clients.",
        ),
        kind: Kind::Select(|_| {
            pairs(&[
                ("html", "Send HTML Email"),
                ("text", "Send Plain Text Email"),
            ])
        }),
        default: |_| json!("html"),
    },
    Setting {
        key: "re-prefix",
        name: "Add \"Re:\" Prefix",
        panel: Some("emailformat"),
        order: 200,
        instructions: Some(RE_PREFIX_INSTRUCTIONS),
        kind: Kind::Select(|_| {
            pairs(&[
                ("re", "Enable \"Re:\" Prefix"),
                ("none", "Disable \"Re:\" Prefix"),
            ])
        }),
        default: |_| json!("none"),
    },
    Setting {
        key: "vary-subject",
        name: "Vary Subjects",
        panel: Some("emailformat"),
        order: 300,
        instructions: Some(VARY_SUBJECT_INSTRUCTIONS),
        kind: Kind::Select(|_| {
            pairs(&[
                ("vary", "Enable Vary Subjects"),
                ("static", "Disable Vary Subjects"),
            ])
        }),
        default: |_| json!("vary"),
    },
    Setting {
        key: "stamps",
        name: "Send Stamps",
        panel: Some("emailformat"),
        order: 400,
        instructions: Some(STAMPS_INSTRUCTIONS),
        kind: Kind::Select(|_| {
            pairs(&[
                ("header", "Mail Headers"),
                ("body", "Mail Headers and Body"),
            ])
        }),
        default: |_| json!("header"),
    },
    Setting {
        key: MAIL_TAGS,
        name: "Mail Tags",
        panel: None,
        order: 1000,
        instructions: None,
        kind: Kind::Internal,
        default: |_| json!([]),
    },
];

/// The setting with `key`.
pub fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|setting| setting.key == key)
}

/// The settings of a panel, in their order (`getSettingOrderVector()`).
pub fn for_panel(panel: &str) -> Vec<&'static Setting> {
    let mut settings: Vec<&Setting> = SETTINGS
        .iter()
        .filter(|setting| setting.panel == Some(panel))
        .collect();
    settings.sort_by_key(|setting| (setting.order, setting.name));
    settings
}

/// The array key PHP turns a value into, or `None` for arrays.
fn php_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some(String::new()),
        Value::Bool(flag) => Some(if *flag { "1" } else { "0" }.to_owned()),
        Value::Number(number) => Some(match number.as_i64() {
            Some(integer) => integer.to_string(),
            // Floats are truncated to integers.
            #[expect(clippy::cast_possible_truncation, reason = "PHP truncates")]
            None => (number.as_f64().unwrap_or_default().trunc() as i64).to_string(),
        }),
        Value::String(text) => Some(text.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

impl Setting {
    /// Whether Rust can show and save this setting as PHP would: the
    /// translations offered in developer mode aren't ported.
    pub fn is_supported(&self, context: &Context<'_>) -> bool {
        !(self.key == TRANSLATION && context.config.truthy("phabricator.developer-mode"))
    }

    /// `getSettingDefaultValue()`.
    pub fn default_value(&self, context: &Context<'_>) -> Value {
        (self.default)(context)
    }

    /// The options, flat, the first of duplicate values winning
    /// (`getSelectOptions()`, `getSelectOptionMap()`).
    fn option_map(&self, context: &Context<'_>) -> Vec<(String, String)> {
        match self.kind {
            Kind::Select(options) => options(context),
            Kind::Groups(groups) => {
                let mut map: Vec<(String, String)> = Vec::new();
                for (key, label) in groups(context).into_iter().flat_map(|group| group.options) {
                    if !map.iter().any(|(known, _)| *known == key) {
                        map.push((key, label));
                    }
                }
                map
            }
            Kind::Text(_) | Kind::Internal => Vec::new(),
        }
    }

    /// The control of the form, `default` being the value used when the
    /// user has none (the global default or the setting's own).
    pub fn control(&self, context: &Context<'_>, default: &Value) -> Option<Control> {
        let map = self.option_map(context);
        let default_label = || {
            let label = php_key(default)
                .and_then(|key| map.iter().find(|(option, _)| *option == key))
                .map(|(_, label)| label.clone());
            match label {
                Some(label) => format!("Default ({label})"),
                None => format!("Default (Unknown, \"{}\")", php::string_cast(default)),
            }
        };
        match self.kind {
            Kind::Select(options) => {
                let mut options = options(context);
                // `empty($options[''])`.
                let has_empty = options
                    .iter()
                    .any(|(key, label)| key.is_empty() && php::is_truthy_str(label));
                if !has_empty {
                    options.retain(|(key, _)| !key.is_empty());
                    options.insert(0, (String::new(), default_label()));
                }
                Some(Control::Select(vec![OptionGroup {
                    label: None,
                    options,
                }]))
            }
            Kind::Groups(groups) => {
                let mut groups = groups(context);
                if let Some(first) = groups.first_mut() {
                    first.options.retain(|(key, _)| !key.is_empty());
                    first.options.insert(0, (String::new(), default_label()));
                }
                Some(Control::Select(groups))
            }
            Kind::Text(_) => Some(Control::Text),
            Kind::Internal => None,
        }
    }

    /// `validateTransactionValue()` of what the form sent; the error says
    /// why PHP refuses it.
    pub fn validate(&self, context: &Context<'_>, value: &Value) -> Result<(), String> {
        let text = php::string_cast(value);
        let valid = match self.kind {
            Kind::Select(_) | Kind::Groups(_) => {
                text.is_empty()
                    || self
                        .option_map(context)
                        .iter()
                        .any(|(key, _)| *key == *text)
            }
            Kind::Text(is_valid) => text.is_empty() || is_valid(&text),
            Kind::Internal => true,
        };
        if valid {
            Ok(())
        } else {
            Err(format!(
                "Value \"{text}\" is not valid for setting \"{}\".",
                self.name
            ))
        }
    }

    /// `getTransactionNewValue()`: what the editor stores, `null` meaning
    /// the setting is removed.
    pub fn new_value(&self, value: &Value) -> Value {
        match self.kind {
            Kind::Select(_) | Kind::Groups(_) | Kind::Text(_) => {
                let text = php::string_cast(value);
                if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text.into_owned())
                }
            }
            Kind::Internal => value.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(config: &Config) -> Context<'_> {
        Context {
            config,
            viewer_name: "alice",
            now: 1_700_000_000,
        }
    }

    #[test]
    fn panels_list_their_settings_in_order() {
        let keys = |panel| {
            for_panel(panel)
                .iter()
                .map(|setting| setting.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys("datetime"),
            ["timezone", "date-format", "time-format", "week-start-day"]
        );
        assert_eq!(keys("language"), ["translation", "pronoun"]);
        assert_eq!(
            keys("display"),
            [
                "resource-postprocessor",
                "titles",
                "monospaced",
                "monospaced-textareas"
            ]
        );
        assert_eq!(keys("emaildelivery"), ["no-mail", "self-mail"]);
    }

    #[test]
    fn selects_offer_the_default_first() {
        let config = Config::default();
        let context = context(&config);
        let Some(Control::Select(groups)) =
            find("week-start-day").unwrap().control(&context, &json!(0))
        else {
            panic!("a select");
        };
        assert_eq!(
            groups[0].options[..2],
            pairs(&[("", "Default (Sunday)"), ("0", "Sunday")])
        );
        let Some(Control::Select(groups)) =
            find("titles").unwrap().control(&context, &json!("nope"))
        else {
            panic!("a select");
        };
        assert_eq!(groups[0].options[0].1, "Default (Unknown, \"nope\")");
        // The pronoun has an empty option of its own.
        let Some(Control::Select(groups)) = find("pronoun").unwrap().control(&context, &json!(""))
        else {
            panic!("a select");
        };
        assert_eq!(groups[0].options[0].1, "alice updated their profile");
        assert_eq!(groups[0].options.len(), 3);
    }

    #[test]
    fn time_zones_are_grouped_by_offset() {
        let config = Config::default();
        let Some(Control::Select(groups)) = find(TIMEZONE)
            .unwrap()
            .control(&context(&config), &json!("UTC"))
        else {
            panic!("a select");
        };
        assert_eq!(groups[0].label.as_deref(), Some("Default"));
        assert_eq!(groups[0].options, pairs(&[("", "Default (UTC)")]));
        let labels: Vec<_> = groups
            .iter()
            .filter_map(|group| group.label.clone())
            .collect();
        assert!(labels.contains(&"UTC+5:30".to_owned()), "{labels:?}");
        assert!(labels.contains(&"UTC-9:30".to_owned()), "{labels:?}");
        let india = groups
            .iter()
            .find(|group| group.label.as_deref() == Some("UTC+5:30"))
            .unwrap();
        assert!(
            india
                .options
                .contains(&("Asia/Kolkata".to_owned(), "Asia/Kolkata".to_owned()))
        );
        let utc = groups
            .iter()
            .find(|group| group.label.as_deref() == Some("UTC+0"))
            .unwrap();
        assert!(utc.options.contains(&(
            "America/Danmarkshavn".to_owned(),
            "America/Danmarkshavn".to_owned()
        )));
        let new_york = groups
            .iter()
            .flat_map(|group| &group.options)
            .find(|(key, _)| key == "America/New_York")
            .unwrap();
        assert_eq!(new_york.1, "America/New York");
    }

    #[test]
    fn values_are_validated_and_stored_like_php() {
        let config = Config::default();
        let context = context(&config);
        let week = find("week-start-day").unwrap();
        assert!(week.validate(&context, &json!("3")).is_ok());
        assert!(week.validate(&context, &json!("")).is_ok());
        assert!(week.validate(&context, &Value::Null).is_ok());
        assert!(week.validate(&context, &json!("03")).is_err());
        assert_eq!(week.new_value(&json!("3")), json!("3"));
        assert_eq!(week.new_value(&json!("")), Value::Null);
        assert_eq!(week.new_value(&Value::Null), Value::Null);
        let font = find("monospaced").unwrap();
        assert!(
            font.validate(&context, &json!("13px \"Fira Code\", mono"))
                .is_ok()
        );
        assert!(font.validate(&context, &json!("13px; color: red")).is_err());
        let zone = find(TIMEZONE).unwrap();
        assert!(zone.validate(&context, &json!("Europe/Paris")).is_ok());
        assert!(zone.validate(&context, &json!("Mars/Olympus")).is_err());
        let tags = find(MAIL_TAGS).unwrap();
        assert_eq!(tags.new_value(&json!({"a": 1})), json!({"a": 1}));
    }
}
