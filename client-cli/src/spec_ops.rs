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
use toml_edit::{value, Array, Item, Table, Value};

// These come from main.rs — re-exported via `use crate::...` when this is a module.
use crate::core::spec::{
    coerce_array_element, coerce_value, load_doc, nk_path, parse_key_path, save_and_validate,
};
use crate::{handle_upgrade_interactive, resolve_alias};

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
