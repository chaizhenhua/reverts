use std::fs;
use std::path::Path;

use reverts_ir::ModuleId;
use reverts_pipeline::SymbolIndexEntry;

pub(crate) fn load_symbol_index(path: &Path) -> Result<Vec<SymbolIndexEntry>, std::io::Error> {
    let text = fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to parse symbol index JSON: {error}"),
        )
    })?;
    let rows = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "symbol index JSON must be an array",
        )
    })?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| symbol_index_entry_from_json(row, index))
        .collect()
}

fn symbol_index_entry_from_json(
    row: &serde_json::Value,
    index: usize,
) -> Result<SymbolIndexEntry, std::io::Error> {
    let field = |name: &str| {
        row.get(name).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("symbol index row {index} is missing `{name}`"),
            )
        })
    };
    let string_field = |name: &str| {
        field(name).and_then(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("symbol index row {index} field `{name}` must be a string"),
                )
            })
        })
    };
    let module_id_u64 = field("module_id").and_then(|value| {
        value.as_u64().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("symbol index row {index} field `module_id` must be a positive integer"),
            )
        })
    })?;
    let module_id = u32::try_from(module_id_u64).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("symbol index row {index} field `module_id` exceeds u32"),
        )
    })?;
    let semantic_named = field("semantic_named").and_then(|value| {
        value.as_bool().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("symbol index row {index} field `semantic_named` must be a boolean"),
            )
        })
    })?;
    let bool_field = |name: &str| {
        row.get(name)
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("symbol index row {index} field `{name}` must be a boolean"),
                    )
                })
            })
            .transpose()
            .map(|value| value.unwrap_or(false))
    };
    let function_like = bool_field("function_like")?;
    let dead = bool_field("dead")?;
    Ok(SymbolIndexEntry {
        module_id: ModuleId(module_id),
        original_name: string_field("original_name")?,
        emitted_name: string_field("emitted_name")?,
        semantic_named,
        file_path: string_field("file_path")?,
        function_like,
        dead,
    })
}
