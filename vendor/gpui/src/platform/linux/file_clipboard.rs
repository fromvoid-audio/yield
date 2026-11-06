use std::{
    os::unix::ffi::OsStrExt,
    path::{Component, PathBuf},
};

use anyhow::{Result, anyhow, bail};
use http_client::Url;

const MAX_FILES: usize = 256;
const MAX_BYTES: usize = 1024 * 1024;

pub(super) struct FileClipboardPayload {
    pub(super) uri_list: Vec<u8>,
    pub(super) gnome_copied_files: Vec<u8>,
}

impl FileClipboardPayload {
    pub(super) fn from_paths(paths: &[PathBuf]) -> Result<Self> {
        if paths.is_empty() || paths.len() > MAX_FILES {
            bail!("File clipboard requires 1 to {MAX_FILES} paths");
        }
        let mut input_bytes = 0usize;
        let mut uri_list = Vec::new();
        let mut gnome_copied_files = b"copy".to_vec();
        for path in paths {
            input_bytes = input_bytes.saturating_add(path.as_os_str().as_bytes().len());
            if input_bytes > MAX_BYTES {
                bail!("File clipboard paths exceed the byte limit");
            }
            if path.components().any(|part| part == Component::ParentDir) {
                bail!("File clipboard requires resolved absolute paths");
            }
            let url = Url::from_file_path(path)
                .map_err(|_| anyhow!("File clipboard requires resolved absolute paths"))?;
            let uri = url.as_str().as_bytes();
            if uri_list.len() + uri.len() + 2 > MAX_BYTES {
                bail!("File clipboard URLs exceed the byte limit");
            }
            uri_list.extend_from_slice(uri);
            uri_list.extend_from_slice(b"\r\n");
            gnome_copied_files.push(b'\n');
            gnome_copied_files.extend_from_slice(uri);
        }
        Ok(Self {
            uri_list,
            gnome_copied_files,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::Path};

    use super::*;

    #[test]
    fn file_clipboard_encodes_ordered_uris_and_gnome_copy_bytes() {
        let paths = [
            PathBuf::from("/audio/é kick #1.wav"),
            Path::new(OsStr::from_bytes(b"/audio/byte-\xff\n.wav")).to_path_buf(),
        ];
        let payload = FileClipboardPayload::from_paths(&paths).unwrap();
        assert_eq!(
            payload.uri_list,
            b"file:///audio/%C3%A9%20kick%20%231.wav\r\nfile:///audio/byte-%FF%0A.wav\r\n"
        );
        assert_eq!(
            payload.gnome_copied_files,
            b"copy\nfile:///audio/%C3%A9%20kick%20%231.wav\nfile:///audio/byte-%FF%0A.wav"
        );
    }

    #[test]
    fn file_clipboard_rejects_relative_parent_empty_and_oversized_lists() {
        assert!(FileClipboardPayload::from_paths(&[]).is_err());
        assert!(FileClipboardPayload::from_paths(&["relative.wav".into()]).is_err());
        assert!(FileClipboardPayload::from_paths(&["/a/../kick.wav".into()]).is_err());
        assert!(FileClipboardPayload::from_paths(&vec!["/a".into(); MAX_FILES + 1]).is_err());
        let encoded_too_large = PathBuf::from(format!("/{}", " ".repeat(MAX_BYTES / 2)));
        assert!(FileClipboardPayload::from_paths(&[encoded_too_large]).is_err());
    }
}
