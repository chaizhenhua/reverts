//! Relative-path computation for module import specifiers.
//!
//! The planner deals in POSIX-style logical paths (e.g.
//! `modules/runtime/source-12-helpers.ts`). When one planned file
//! needs to import another, the resulting specifier has to be a
//! relative path expressed in POSIX form because it ends up inside an
//! `import` literal. `relative_import_specifier` does that with one
//! additional convention: a recovered `.ts`/`.tsx` filename on the *target*
//! is rewritten to `.js` because the emitted runtime treats the
//! generated file as JavaScript source for `import` resolution.
//!
//! These helpers are intentionally string-only; they do not touch
//! `std::path::Path` because that would normalize the platform
//! separator and lose the POSIX guarantees we rely on for import
//! specifiers.

pub(crate) fn relative_import_specifier(from_file: &str, to_file: &str) -> String {
    let from_dir = path_dir_segments(from_file);
    let to_segments = path_file_segments_with_js_extension(to_file);
    let common = common_prefix_len(&from_dir, &to_segments);
    let mut relative = Vec::new();
    relative.extend(std::iter::repeat_n(
        "..".to_string(),
        from_dir.len().saturating_sub(common),
    ));
    relative.extend(to_segments[common..].iter().cloned());
    let joined = relative.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

fn path_dir_segments(path: &str) -> Vec<String> {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    segments.pop();
    segments
}

fn path_file_segments_with_js_extension(path: &str) -> Vec<String> {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last
            .strip_suffix(".tsx")
            .or_else(|| last.strip_suffix(".ts"))
    {
        *last = format!("{stripped}.js");
    }
    segments
}

fn common_prefix_len(left: &[String], right: &[String]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

#[cfg(test)]
mod tests {
    use super::relative_import_specifier;

    #[test]
    fn target_ts_and_tsx_extensions_are_rewritten_for_runtime_imports() {
        assert_eq!(
            relative_import_specifier("modules/entrypoint.ts", "components/Button.ts"),
            "../components/Button.js"
        );
        assert_eq!(
            relative_import_specifier("modules/entrypoint.ts", "components/Button.tsx"),
            "../components/Button.js"
        );
    }
}
