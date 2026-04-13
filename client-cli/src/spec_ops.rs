// spec_ops.rs — CLI helpers for editing & deleting .nk specs in-place.
//
// Add to main.rs (top-level):
//     mod spec_ops;
//     use spec_ops::{handle_spec_set, handle_spec_unset, handle_spec_delete};
//
// Uses toml_edit to preserve comments + formatting in the .nk file.

use colored::Colorize;
use dialoguer::{theme::ColorfulTheme, Confirm};
use std::error::Error;
use std::path::PathBuf;
use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

// These come from main.rs — re-exported via `use crate::...` when this is a module.
use crate::{handle_upgrade_interactive, nk_path, resolve_alias, DeploymentSpec};

/// Known top-level tables in a DeploymentSpec. Used to validate dotted keys
/// so a typo like `deployment.imag` doesn't silently create a dead field.
const KNOWN_TABLES: &[&str] = &["deployment", "resources", "network", "storage", "placement"];

fn load_doc(name: &str) -> Result<(PathBuf, DocumentMut), Box<dyn Error>> {
    let path = nk_path(name);
    if !path.exists() {
        return Err(format!(
            "No .nk spec found for '{}'. Run 'nordkraft init {}' first.",
            name, name
        )
        .into());
    }
    let contents = std::fs::read_to_string(&path)?;
    let doc: DocumentMut = contents
        .parse()
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    Ok((path, doc))
}

fn save_and_validate(path: &PathBuf, doc: &DocumentMut) -> Result<(), Box<dyn Error>> {
    let serialized = doc.to_string();
    // Round-trip through DeploymentSpec to catch type errors early.
    if let Err(e) = toml::from_str::<DeploymentSpec>(&serialized) {
        return Err(format!(
            "Refusing to save: result would be an invalid spec.\n  → {}",
            e
        )
        .into());
    }
    std::fs::write(path, serialized)?;
    Ok(())
}

/// Parse a dotted key path like "resources.cpu" or "deployment.image" into
/// (table, field). Single-segment keys are rejected — every field lives under
/// a table in our schema.
fn parse_key_path(key: &str) -> Result<(&str, &str), Box<dyn Error>> {
    let parts: Vec<&str> = key.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Key must be dotted (e.g. 'resources.cpu', 'deployment.image'). Got: '{}'",
            key
        )
        .into());
    }
    let (table, field) = (parts[0], parts[1]);
    if !KNOWN_TABLES.contains(&table) {
        return Err(format!(
            "Unknown table '{}'. Known tables: {}",
            table,
            KNOWN_TABLES.join(", ")
        )
        .into());
    }
    Ok((table, field))
}

/// Coerce a CLI string into the right TOML type by trying int → float → bool → string.
fn coerce_value(raw: &str) -> Item {
    if let Ok(i) = raw.parse::<i64>() {
        return value(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return value(f);
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" => return value(true),
        "false" => return value(false),
        _ => {}
    }
    value(raw)
}

/// Same coercion but returns a `Value` suitable for pushing into an Array.
fn coerce_array_element(raw: &str) -> Value {
    if let Ok(i) = raw.parse::<i64>() {
        return Value::from(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Value::from(f);
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" => return Value::from(true),
        "false" => return Value::from(false),
        _ => {}
    }
    Value::from(raw)
}

/// `nordkraft spec set <app> <key> <value> [--apply]`
///
/// Supports three value modes on the RHS:
///   plain          → replace
///   +item          → append to array (e.g. `network.ports +8080:80`)
///   -item          → remove from array
pub async fn handle_spec_set(
    container: String,
    key: String,
    raw_value: String,
    apply: bool,
    _json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let name = resolve_alias(&container);
    let (path, mut doc) = load_doc(&name)?;
    let (table, field) = parse_key_path(&key)?;

    let table_tbl: &mut Table = doc
        .as_table_mut()
        .get_mut(table)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("Table '[{}]' missing from spec", table))?;

    // Array append / remove
    if let Some(stripped) = raw_value.strip_prefix('+') {
        let existing = table_tbl.entry(field).or_insert(value(Array::new()));
        let arr = existing
            .as_array_mut()
            .ok_or_else(|| format!("'{}' is not an array — cannot append", key))?;
        arr.push(coerce_array_element(stripped));
        println!("{} {} += {}", "✔".green(), key.bold(), stripped);
    } else if let Some(stripped) = raw_value.strip_prefix('-') {
        let existing = table_tbl
            .get_mut(field)
            .ok_or_else(|| format!("'{}' does not exist", key))?;
        let arr = existing
            .as_array_mut()
            .ok_or_else(|| format!("'{}' is not an array — cannot remove from", key))?;
        let before = arr.len();
        // Compare against each element's display form so typed arrays
        // (Vec<u16>, Vec<String>, Vec<bool>) all work with the same `-value` syntax.
        arr.retain(|v: &Value| {
            let as_str = v.as_str().map(|s| s.to_string());
            let as_int = v.as_integer().map(|i| i.to_string());
            let as_float = v.as_float().map(|f| f.to_string());
            let as_bool = v.as_bool().map(|b| b.to_string());
            let repr = as_str.or(as_int).or(as_float).or(as_bool);
            repr.as_deref() != Some(stripped)
        });
        if arr.len() == before {
            return Err(format!("'{}' not found in {}", stripped, key).into());
        }
        println!("{} {} -= {}", "✔".green(), key.bold(), stripped);
    } else {
        // Replace
        if !table_tbl.contains_key(field) {
            eprintln!(
                "{} '{}' did not exist before — creating new field.",
                "ℹ".cyan(),
                key
            );
        }
        table_tbl[field] = coerce_value(&raw_value);
        println!("{} {} = {}", "✔".green(), key.bold(), raw_value);
    }

    save_and_validate(&path, &doc)?;
    println!("   {} {}", "Saved:".dimmed(), path.display());

    if apply {
        println!();
        println!("{}", "→ Applying to running container…".cyan());
        handle_upgrade_interactive(Some(name), true, false).await?;
    } else {
        println!(
            "   {} nordkraft upgrade {}",
            "Apply with:".dimmed(),
            container
        );
    }
    Ok(())
}

/// `nordkraft spec unset <app> <key>`
pub async fn handle_spec_unset(
    container: String,
    key: String,
    _json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let name = resolve_alias(&container);
    let (path, mut doc) = load_doc(&name)?;
    let (table, field) = parse_key_path(&key)?;

    let table_tbl: &mut Table = doc
        .as_table_mut()
        .get_mut(table)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("Table '[{}]' missing", table))?;

    if table_tbl.remove(field).is_none() {
        return Err(format!("'{}' was not set", key).into());
    }

    save_and_validate(&path, &doc)?;
    println!("{} removed {}", "✔".green(), key.bold());
    println!("   {} {}", "Saved:".dimmed(), path.display());
    Ok(())
}

/// `nordkraft spec delete <app> [--yes]`
///
/// Deletes the .nk file only. Does NOT touch the running container — that's
/// `nordkraft destroy`. We print a reminder if a spec is removed while a
/// container by the same name might still exist.
pub async fn handle_spec_delete(
    container: String,
    yes: bool,
    _json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let name = resolve_alias(&container);
    let path = nk_path(&name);
    if !path.exists() {
        return Err(format!("No .nk spec found for '{}'", name).into());
    }

    if !yes {
        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Delete spec file for '{}'? (container will NOT be destroyed)",
                name
            ))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !confirmed {
            println!("{}", "   Cancelled.".dimmed());
            return Ok(());
        }
    }

    std::fs::remove_file(&path)?;
    println!("{} deleted {}", "✔".green(), path.display());
    println!(
        "   {} If '{}' is still running, destroy it with: nordkraft destroy {}",
        "Note:".yellow(),
        name,
        name
    );
    Ok(())
}
