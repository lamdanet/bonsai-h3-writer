//! Small helpers for the desktop UI: opening files/folders/links and native file dialogs.

use std::path::{Path, PathBuf};

use eframe::egui;

/// Open a folder or file with the default program (Explorer for folders).
pub fn open_path(path: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Quoted by hand (Rust quotes only paths with spaces): Explorer cuts an unquoted path at commas.
        std::process::Command::new("explorer").raw_arg(explorer_quoted(path)).spawn()?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}

/// Show a file selected in Explorer.
pub fn reveal_in_explorer(path: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `.arg` would quote the whole `/select,<path>` token, and Explorer then ignores the switch.
        std::process::Command::new("explorer").raw_arg(format!("/select,{}", explorer_quoted(path))).spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        open_path(path.parent().unwrap_or(path))
    }
}

/// `path` in double quotes for Explorer's own command-line parser (Windows paths cannot contain
/// `"`). A trailing backslash is dropped, except a drive root's, so it cannot escape the quote.
#[cfg(windows)]
fn explorer_quoted(path: &Path) -> String {
    let text = path.display().to_string();
    let trimmed = text.trim_end_matches('\\');
    let text = if trimmed.is_empty() || trimmed.ends_with(':') { text.as_str() } else { trimmed };
    format!("\"{text}\"")
}

/// Open a web link in the browser.
pub fn open_url(ctx: &egui::Context, url: &str) {
    ctx.open_url(egui::OpenUrl::new_tab(url));
}

/// Native "choose folder" dialog.
pub fn pick_folder(title: &str, start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(dir) = start.filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

/// Native "open image" dialog (one file).
pub fn pick_image(title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title(title)
        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
        .pick_file()
}

/// Native "open images" dialog (several files).
pub fn pick_images(title: &str) -> Vec<PathBuf> {
    rfd::FileDialog::new()
        .set_title(title)
        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
        .pick_files()
        .unwrap_or_default()
}

/// Native "save as" dialog for a text file.
pub fn pick_save_txt(title: &str, default_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).set_file_name(default_name).add_filter("Text", &["txt"]).save_file()
}

/// Write text as UTF-8 with BOM and CRLF line endings (opens correctly in Notepad).
pub fn write_text_for_windows(path: &Path, text: &str) -> anyhow::Result<()> {
    let body = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(body.as_bytes());
    std::fs::write(path, bytes)?;
    Ok(())
}

/// `file://` URI for egui's image loader.
pub fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// File-name-safe slug from a title ("6 s walk, film look" -> "6-s-walk-film-look").
pub fn slug(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= max_len {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "prompt".to_string()
    } else {
        trimmed
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn explorer_paths_are_quoted() {
        let shot = Path::new(r"C:\Users\Jane Doe\Pictures\Screenshot 2026-10-28 101500.png");
        assert_eq!(explorer_quoted(shot), r#""C:\Users\Jane Doe\Pictures\Screenshot 2026-10-28 101500.png""#);
        assert_eq!(explorer_quoted(Path::new(r"D:\refs\a,b.png")), r#""D:\refs\a,b.png""#);
        assert_eq!(explorer_quoted(Path::new(r"D:\refs\")), r#""D:\refs""#);
        assert_eq!(explorer_quoted(Path::new(r"C:\")), r#""C:\""#);
    }
}
