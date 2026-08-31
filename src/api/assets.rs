//! The local interface's static assets, compiled into the binary.
//!
//! The product says the browser interface "is compiled to static assets and
//! embedded into the binary; it is not a separately deployed application", so
//! the table below is the whole deployment story: three files, resolved at
//! compile time, served by the same process that owns the database.
//!
//! The table is written out by hand rather than walking a directory at build
//! time. A directory walk would embed whatever happened to be in the folder —
//! an editor's swap file, a stray screenshot, a `.env` someone dropped there
//! while debugging — into a binary that gets copied around. An explicit list
//! cannot do that, and it costs one line per asset.

/// One embedded file: its request path, its media type, and its bytes.
pub struct Asset {
    /// The path the browser requests, including the leading slash.
    pub path: &'static str,
    /// The `Content-Type` to serve it with, including the charset.
    pub content_type: &'static str,
    /// The file's contents.
    pub body: &'static [u8],
}

/// The interface shell. Served for every navigation into the application.
pub const INDEX: Asset = Asset {
    path: "/index.html",
    content_type: "text/html; charset=utf-8",
    body: include_bytes!("../../web/static/index.html"),
};

/// Every embedded asset, including the shell.
pub const ASSETS: &[Asset] = &[
    INDEX,
    Asset {
        path: "/assets/app.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../../web/static/app.css"),
    },
    Asset {
        path: "/assets/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../../web/static/app.js"),
    },
];

/// Look up an asset by its exact request path.
///
/// Exact match, no path joining and no normalization: an embedded table has no
/// filesystem beneath it, so there is nothing for `..` to escape into, and
/// keeping the lookup a comparison means it stays that way if the table ever
/// grows a nested path.
pub fn lookup(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod accessibility;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_is_present_and_addressable() {
        for asset in ASSETS {
            assert!(!asset.body.is_empty(), "{} is empty", asset.path);
            assert!(
                lookup(asset.path).is_some(),
                "{} is not addressable",
                asset.path
            );
        }
        assert!(lookup("/assets/../../etc/passwd").is_none());
        assert!(lookup("/assets/app.js/").is_none());
    }

    /// Every element and template the script reaches for must exist.
    ///
    /// The page is built by cloning declared templates and filling elements
    /// looked up by id, so the failure mode that matters is a rename on one
    /// side of that pair. In a browser it surfaces as a silently blank panel
    /// or a `null` dereference on one route; here it is a failing build.
    ///
    /// The extraction is deliberately literal — `$("id")` and
    /// `clone("tpl-id")` with a quoted constant — so a lookup built from a
    /// variable is simply not checked rather than checked wrongly. Every such
    /// lookup in the script is a constant today, and the count assertion is
    /// what would notice if that stopped being true.
    #[test]
    fn every_element_the_script_looks_up_exists_in_the_shell() {
        let html = std::str::from_utf8(INDEX.body).unwrap();
        let script = std::str::from_utf8(
            ASSETS
                .iter()
                .find(|asset| asset.path == "/assets/app.js")
                .unwrap()
                .body,
        )
        .unwrap();

        let declared: Vec<&str> = html
            .match_indices("id=\"")
            .map(|(index, _)| {
                let rest = &html[index + 4..];
                &rest[..rest.find('"').expect("a closed attribute")]
            })
            .collect();

        let mut checked = 0usize;
        for call in ["$(\"", "clone(\""] {
            for (index, _) in script.match_indices(call) {
                let rest = &script[index + call.len()..];
                let Some(end) = rest.find('"') else { continue };
                let id = &rest[..end];
                // A lookup interrupted by anything but an identifier is a
                // string that merely starts the same way, not an id.
                if id.is_empty() || !id.chars().all(|c| c.is_alphanumeric() || c == '-') {
                    continue;
                }
                checked += 1;
                assert!(
                    declared.contains(&id),
                    "the script looks up `{id}`, which the shell does not declare"
                );
            }
        }
        assert!(checked >= 40, "only {checked} lookups were checked");

        // And the other direction for templates: one that nothing clones is
        // dead markup shipped in every binary.
        for id in declared.iter().filter(|id| id.starts_with("tpl-")) {
            assert!(
                script.contains(&format!("clone(\"{id}\")")),
                "the shell declares template `{id}`, which nothing clones"
            );
        }
    }

    #[test]
    fn the_shell_references_only_embedded_assets() {
        let html = std::str::from_utf8(INDEX.body).unwrap();
        for attribute in ["src=\"", "href=\""] {
            for (index, _) in html.match_indices(attribute) {
                let rest = &html[index + attribute.len()..];
                let value = &rest[..rest.find('"').expect("a closed attribute")];
                if value.starts_with('#') {
                    continue;
                }
                assert!(
                    lookup(value).is_some(),
                    "the shell references {value}, which is not an embedded asset"
                );
            }
        }
    }
}
