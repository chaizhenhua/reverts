use reverts_ir::{ByteRange, ModuleId};

/// Bundler wrapper shape encountered around a module body.
///
/// Distinct from `reverts_analyze::CompilerKind` because a single bundle
/// can be produced by webpack yet contain `define(...)` AMD modules
/// inside; we record what wrapper shape was actually decoded for each
/// inner module rather than the toolchain that emitted the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BundlerKind {
    Esbuild,
    Webpack4,
    Webpack5,
    RollupCjs,
    RollupEsm,
    Umd,
    Browserify,
    Amd,
}

impl BundlerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Esbuild => "esbuild",
            Self::Webpack4 => "webpack4",
            Self::Webpack5 => "webpack5",
            Self::RollupCjs => "rollup_cjs",
            Self::RollupEsm => "rollup_esm",
            Self::Umd => "umd",
            Self::Browserify => "browserify",
            Self::Amd => "amd",
        }
    }
}

/// An inner module extracted from a bundle. `body_span` always covers
/// a parseable program unit — never a mid-expression fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerModule {
    /// Stable identifier within the parent bundle. Strategies:
    /// - esbuild `__commonJS`: the registration key, e.g. `"node_modules/lodash/index.js"`
    /// - webpack: the module id (string or number stringified)
    /// - rollup / umd / fallback: `"<bundler>:<seq>"`, e.g. `"rollup_cjs:0"`.
    pub virtual_id: String,
    /// Byte range of the body inside the parent file's source. Always
    /// slices a parseable JavaScript program unit.
    pub body_span: ByteRange,
    /// Wrapper shape decoded for this module.
    pub bundler: BundlerKind,
    /// Source path hint when the bundler embeds it as the registration
    /// key. None when the bundler uses numeric ids or anonymous shapes.
    pub source_path_hint: Option<String>,
    /// Parent module that contains this inner.
    pub parent_module_id: ModuleId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundler_kind_as_str_returns_expected_strings() {
        assert_eq!(BundlerKind::Esbuild.as_str(), "esbuild");
        assert_eq!(BundlerKind::Webpack4.as_str(), "webpack4");
        assert_eq!(BundlerKind::Webpack5.as_str(), "webpack5");
        assert_eq!(BundlerKind::RollupCjs.as_str(), "rollup_cjs");
        assert_eq!(BundlerKind::RollupEsm.as_str(), "rollup_esm");
        assert_eq!(BundlerKind::Umd.as_str(), "umd");
        assert_eq!(BundlerKind::Browserify.as_str(), "browserify");
        assert_eq!(BundlerKind::Amd.as_str(), "amd");
    }

    #[test]
    fn inner_module_struct_holds_all_fields() {
        let m = InnerModule {
            virtual_id: "esbuild:0".into(),
            body_span: ByteRange::new(100, 500),
            bundler: BundlerKind::Esbuild,
            source_path_hint: Some("node_modules/lodash/index.js".into()),
            parent_module_id: ModuleId(7),
        };
        assert_eq!(m.virtual_id, "esbuild:0");
        assert_eq!(m.body_span.start, 100);
        assert_eq!(m.body_span.end, 500);
        assert_eq!(m.bundler, BundlerKind::Esbuild);
        assert_eq!(
            m.source_path_hint.as_deref(),
            Some("node_modules/lodash/index.js")
        );
        assert_eq!(m.parent_module_id, ModuleId(7));
    }
}
