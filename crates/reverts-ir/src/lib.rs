use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingName(String);

impl BindingName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Application,
    Package,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRecord {
    pub id: ModuleId,
    pub kind: ModuleKind,
    pub semantic_path: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

impl ModuleRecord {
    #[must_use]
    pub fn application(id: ModuleId, semantic_path: impl Into<String>) -> Self {
        Self {
            id,
            kind: ModuleKind::Application,
            semantic_path: semantic_path.into(),
            package_name: None,
            package_version: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingShape {
    Unknown,
    Value,
    PlainObject,
    NamespaceObject,
    EnumObject,
    Callable,
    Constructor,
    ClassLike,
}

impl BindingShape {
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        self.max(other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingConstraintKind {
    Read,
    Call,
    Construct,
    MemberRead,
    MemberWrite,
    EnumInitializer,
    ClassDeclaration,
}

impl BindingConstraintKind {
    #[must_use]
    pub fn required_shape(self) -> BindingShape {
        match self {
            Self::Read => BindingShape::Value,
            Self::Call => BindingShape::Callable,
            Self::Construct => BindingShape::Constructor,
            Self::MemberRead | Self::MemberWrite => BindingShape::NamespaceObject,
            Self::EnumInitializer => BindingShape::EnumObject,
            Self::ClassDeclaration => BindingShape::ClassLike,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingConstraint {
    pub module_id: ModuleId,
    pub binding: BindingName,
    pub kind: BindingConstraintKind,
}

impl BindingConstraint {
    #[must_use]
    pub fn new(
        module_id: ModuleId,
        binding: impl Into<String>,
        kind: BindingConstraintKind,
    ) -> Self {
        Self {
            module_id,
            binding: BindingName::new(binding),
            kind,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DefUseGraph {
    definitions: BTreeSet<(ModuleId, BindingName)>,
    imports: BTreeSet<(ModuleId, BindingName)>,
    reads: BTreeSet<(ModuleId, BindingName)>,
    constraints: Vec<BindingConstraint>,
}

impl DefUseGraph {
    pub fn define(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.definitions
            .insert((module_id, BindingName::new(binding)));
    }

    pub fn import(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.imports.insert((module_id, BindingName::new(binding)));
    }

    pub fn read(&mut self, module_id: ModuleId, binding: impl Into<String>) {
        self.reads.insert((module_id, BindingName::new(binding)));
    }

    pub fn constrain(&mut self, constraint: BindingConstraint) {
        self.reads
            .insert((constraint.module_id, constraint.binding.clone()));
        self.constraints.push(constraint);
    }

    #[must_use]
    pub fn has_definition_or_import(&self, module_id: ModuleId, binding: &BindingName) -> bool {
        self.definitions.contains(&(module_id, binding.clone()))
            || self.imports.contains(&(module_id, binding.clone()))
    }

    #[must_use]
    pub fn unresolved_reads(&self) -> Vec<(ModuleId, BindingName)> {
        self.reads
            .iter()
            .filter(|(module_id, binding)| !self.has_definition_or_import(*module_id, binding))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn constraints(&self) -> &[BindingConstraint] {
        &self.constraints
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BindingShapeSolution {
    shapes: BTreeMap<(ModuleId, BindingName), BindingShape>,
}

impl BindingShapeSolution {
    pub fn add_constraint(&mut self, constraint: &BindingConstraint) {
        let key = (constraint.module_id, constraint.binding.clone());
        let required = constraint.kind.required_shape();
        self.shapes
            .entry(key)
            .and_modify(|shape| *shape = shape.merge(required))
            .or_insert(required);
    }

    #[must_use]
    pub fn shape_of(&self, module_id: ModuleId, binding: &str) -> BindingShape {
        self.shapes
            .get(&(module_id, BindingName::new(binding)))
            .copied()
            .unwrap_or(BindingShape::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSurface {
    pub package_name: String,
    root_importable: bool,
    subpaths: BTreeSet<String>,
}

impl PackageSurface {
    #[must_use]
    pub fn new(package_name: impl Into<String>) -> Self {
        Self {
            package_name: package_name.into(),
            root_importable: false,
            subpaths: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_root_importable(mut self) -> Self {
        self.root_importable = true;
        self
    }

    #[must_use]
    pub fn with_subpath(mut self, subpath: impl Into<String>) -> Self {
        self.subpaths.insert(normalize_subpath(&subpath.into()));
        self
    }

    #[must_use]
    pub fn accepts(&self, specifier: &str) -> bool {
        let Some((package_name, subpath)) = split_bare_specifier(specifier) else {
            return false;
        };
        if package_name != self.package_name {
            return false;
        }
        match subpath {
            None => self.root_importable,
            Some(subpath) => self.subpaths.contains(&normalize_subpath(&subpath)),
        }
    }
}

#[must_use]
pub fn split_bare_specifier(specifier: &str) -> Option<(String, Option<String>)> {
    if specifier.starts_with('.') || specifier.starts_with('/') || specifier.is_empty() {
        return None;
    }

    let parts = specifier.split('/').collect::<Vec<_>>();
    if specifier.starts_with('@') {
        if parts.len() < 2 || parts[0].len() <= 1 || parts[1].is_empty() {
            return None;
        }
        let package = format!("{}/{}", parts[0], parts[1]);
        let subpath = (parts.len() > 2).then(|| parts[2..].join("/"));
        Some((package, subpath))
    } else {
        let package = parts[0].to_string();
        let subpath = (parts.len() > 1).then(|| parts[1..].join("/"));
        Some((package, subpath))
    }
}

#[must_use]
pub fn is_valid_package_name(value: &str) -> bool {
    let Some((package_name, subpath)) = split_bare_specifier(value) else {
        return false;
    };
    subpath.is_none()
        && package_name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'/' | b'.' | b'_' | b'-')
        })
}

fn normalize_subpath(subpath: &str) -> String {
    subpath.trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use super::{BindingShape, PackageSurface, is_valid_package_name};

    #[test]
    fn package_surface_does_not_accept_absent_subpath() {
        let surface = PackageSurface::new("lodash").with_root_importable();

        assert!(surface.accepts("lodash"));
        assert!(!surface.accepts("lodash/_mapCacheProto.js"));
    }

    #[test]
    fn invalid_uppercase_package_name_is_rejected() {
        assert!(!is_valid_package_name("@smithy/XY7"));
        assert!(!is_valid_package_name("vscode-languageserver-XY7"));
        assert!(is_valid_package_name("@smithy/protocol-http"));
    }

    #[test]
    fn binding_shape_prefers_callable_over_plain_object() {
        assert_eq!(
            BindingShape::PlainObject.merge(BindingShape::Callable),
            BindingShape::Callable
        );
    }
}
