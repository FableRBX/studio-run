//! Builds the throwaway plugin that studio-run drops into Studio's Plugins
//! folder, and cleans up plugins left behind by runs that didn't exit cleanly.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;

use crate::server;

const BOOTSTRAP_TEMPLATE: &str = include_str!("plugin.luau");
const FILE_PREFIX: &str = "studio-run-";
const FILE_EXTENSION: &str = "rbxmx";

pub struct Plugin<'a> {
    pub port: u16,
    pub token: &'a str,
    pub script_name: &'a str,
    pub script_source: &'a str,
}

impl Plugin<'_> {
    /// Serializes the plugin as an XML model: a `Script` that runs the
    /// bootstrap code, with the user's script as a `ModuleScript` child.
    pub fn to_rbxmx(&self) -> String {
        let bootstrap = BOOTSTRAP_TEMPLATE
            .replace("{{PORT}}", &self.port.to_string())
            .replace("{{TOKEN}}", self.token);

        let mut xml = String::from("<roblox version=\"4\">\n");
        xml.push_str("\t<Item class=\"Script\" referent=\"RBX0\">\n");
        push_properties(&mut xml, "StudioRun", &bootstrap);
        xml.push_str("\t\t<Item class=\"ModuleScript\" referent=\"RBX1\">\n");
        push_properties(&mut xml, self.script_name, &wrap_script(self.script_source));
        xml.push_str("\t\t</Item>\n");
        xml.push_str("\t</Item>\n");
        xml.push_str("</roblox>\n");
        xml
    }
}

/// Turns a script into a module that returns it as a function, so the
/// bootstrap can `require` it and catch its errors. The wrapper shares the
/// script's first line so line numbers in errors match the original file.
fn wrap_script(source: &str) -> String {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let shebang_guard = if source.starts_with("#!") { "--" } else { "" };

    format!("return function(plugin, ...) {shebang_guard}{source}\nend\n")
}

fn push_properties(xml: &mut String, name: &str, source: &str) {
    let _ = write!(
        xml,
        "\t\t<Properties>\n\
         \t\t\t<string name=\"Name\">{}</string>\n\
         \t\t\t<ProtectedString name=\"Source\">{}</ProtectedString>\n\
         \t\t</Properties>\n",
        escape_xml(name),
        escape_xml(source),
    );
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());

    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\t' | '\n' => escaped.push(c),
            // Parsers normalize raw carriage returns away, so encode them to
            // keep the source byte-for-byte.
            c if c.is_control() => {
                let _ = write!(escaped, "&#{};", u32::from(c));
            }
            c => escaped.push(c),
        }
    }

    escaped
}

pub fn file_name(port: u16, session_id: &str) -> String {
    format!("{FILE_PREFIX}{port}-{session_id}.{FILE_EXTENSION}")
}

/// A plugin file that is deleted when this guard is dropped.
pub struct InstalledPlugin {
    path: PathBuf,
}

impl InstalledPlugin {
    pub fn install(plugins_dir: &Path, file_name: &str, contents: &str) -> anyhow::Result<Self> {
        fs::create_dir_all(plugins_dir).with_context(|| {
            format!(
                "Could not create the Studio plugins folder at {}",
                plugins_dir.display()
            )
        })?;

        let path = plugins_dir.join(file_name);
        fs::write(&path, contents).with_context(|| {
            format!(
                "Could not write the studio-run plugin to {}",
                path.display()
            )
        })?;

        crate::debug!("installed plugin at {}", path.display());
        Ok(Self { path })
    }
}

impl Drop for InstalledPlugin {
    fn drop(&mut self) {
        match fs::remove_file(&self.path) {
            Ok(()) => crate::debug!("removed plugin {}", self.path.display()),
            Err(err) => crate::warning!(
                "could not remove the studio-run plugin at {}: {err}",
                self.path.display()
            ),
        }
    }
}

/// Deletes plugins from earlier runs that were killed before they could clean
/// up. A plugin is kept if the run that owns it still answers on its port, so
/// runs happening in parallel are left alone.
pub fn remove_stale(plugins_dir: &Path) {
    let Ok(entries) = fs::read_dir(plugins_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some((port, session_id)) = file_name.to_str().and_then(parse_file_name) else {
            continue;
        };

        if server::is_session_alive(port, session_id) {
            continue;
        }

        let path = entry.path();
        match fs::remove_file(&path) {
            Ok(()) => crate::debug!("removed stale plugin {}", path.display()),
            Err(err) => crate::debug!("could not remove stale plugin {}: {err}", path.display()),
        }
    }
}

fn parse_file_name(name: &str) -> Option<(u16, &str)> {
    let rest = name
        .strip_prefix(FILE_PREFIX)?
        .strip_suffix(FILE_EXTENSION)?
        .strip_suffix('.')?;
    let (port, session_id) = rest.split_once('-')?;

    let is_session_id = !session_id.is_empty() && session_id.bytes().all(|b| b.is_ascii_hexdigit());
    is_session_id.then_some((port.parse().ok()?, session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_script_keeps_line_numbers() {
        let wrapped = wrap_script("print(1)\nprint(2)");
        assert_eq!(
            wrapped,
            "return function(plugin, ...) print(1)\nprint(2)\nend\n"
        );
    }

    #[test]
    fn wrapped_script_drops_bom_and_comments_out_shebang() {
        assert_eq!(
            wrap_script("\u{feff}#!/usr/bin/env lune\nprint(1)"),
            "return function(plugin, ...) --#!/usr/bin/env lune\nprint(1)\nend\n"
        );
    }

    #[test]
    fn escapes_xml_special_characters() {
        assert_eq!(
            escape_xml("if a < b and c > d then s = \"&\" end\r\n\t"),
            "if a &lt; b and c &gt; d then s = &quot;&amp;&quot; end&#13;\n\t"
        );
    }

    #[test]
    fn cdata_terminator_cannot_escape_the_source() {
        let plugin = Plugin {
            port: 1234,
            token: "abc",
            script_name: "test",
            script_source: "print(\"]]></ProtectedString>\")",
        };
        let xml = plugin.to_rbxmx();

        assert!(xml.contains("print(&quot;]]&gt;&lt;/ProtectedString&gt;&quot;)"));
        assert_eq!(xml.matches("</ProtectedString>").count(), 2);
    }

    #[test]
    fn bootstrap_placeholders_are_filled() {
        let plugin = Plugin {
            port: 4321,
            token: "deadbeef",
            script_name: "x",
            script_source: "",
        };
        let xml = plugin.to_rbxmx();

        assert!(xml.contains("http://127.0.0.1:4321"));
        assert!(xml.contains("deadbeef"));
        assert!(!xml.contains("{{"));
    }

    #[test]
    fn parses_only_our_file_names() {
        assert_eq!(
            parse_file_name(&file_name(50312, "a1b2c3")),
            Some((50312, "a1b2c3"))
        );
        assert_eq!(parse_file_name("studio-run-50312-a1b2c3.rbxm"), None);
        assert_eq!(parse_file_name("studio-run-notaport-a1b2c3.rbxmx"), None);
        assert_eq!(parse_file_name("studio-run-50312-.rbxmx"), None);
        assert_eq!(parse_file_name("studio-run-50312-xyz.rbxmx"), None);
        assert_eq!(parse_file_name("Rojo.rbxm"), None);
    }
}
