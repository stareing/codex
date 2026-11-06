//! Shared fixtures for the catalog tests: zip bundles, a scripted prompter, a signed-in home.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;

use codex_app_server_transport::SiteAccount;
use zip::write::SimpleFileOptions;

use super::client::CatalogError;
use super::install::Prompter;

pub(crate) const SKILL_MD: &[u8] =
    b"---\nname: pdf-helper\ndescription: Works with PDFs\n---\n\nUse it.\n";

/// A zip with the given files (in order).
pub(crate) fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

pub(crate) fn zip_with_symlink(link: &str, target: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("SKILL.md", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(SKILL_MD).unwrap();
    writer
        .add_symlink(link, target, SimpleFileOptions::default())
        .unwrap();
    writer.finish().unwrap().into_inner()
}

/// Answers from a script; panics when asked something unscripted.
#[derive(Default)]
pub(crate) struct ScriptedPrompter {
    pub(crate) interactive: bool,
    pub(crate) confirms: VecDeque<bool>,
    pub(crate) secrets: VecDeque<(String, String)>,
    pub(crate) asked: Vec<String>,
}

impl Prompter for ScriptedPrompter {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn confirm(&mut self, question: &str) -> Result<bool, CatalogError> {
        self.asked.push(question.to_string());
        Ok(self.confirms.pop_front().expect("unexpected confirmation"))
    }

    fn secret(&mut self, name: &str) -> Result<Option<String>, CatalogError> {
        self.asked.push(name.to_string());
        let (expected, value) = self.secrets.pop_front().expect("unexpected secret prompt");
        assert_eq!(expected, name);
        Ok(Some(value))
    }
}

/// Saves a site account for `site` with `token` under `home`, as `login` would.
pub(crate) fn sign_in(home: &Path, site: &str, token: &str) {
    SiteAccount {
        site_url: site.to_string(),
        relay_url: format!("{site}/agent-relay/"),
        account_id: "acct".to_string(),
        device_id: 7,
        name: "test pc".to_string(),
        model: None,
    }
    .save(home, token)
    .unwrap();
}
