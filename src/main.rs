#![feature(strict_overflow_ops)]
#![feature(iterator_try_collect)]

mod cli;

use anyhow::{Context as _, anyhow};
use clap::Parser as _;
use cli::{
    Cli,
    Commands::{Build, Diff, Upgrade},
};
use colored::Colorize as _;
use serde::Deserialize;
use std::{
    collections::HashSet,
    env, fs,
    io::stdin,
    path::PathBuf,
    process::{Command, exit},
    sync::LazyLock,
};
use toml::Table;

#[expect(clippy::expect_used)]
static CONFIG_PATH: LazyLock<String> = LazyLock::new(|| {
    let home = env::var("HOME").expect("HOME is not set");
    format!("{home}/.config/meta")
});

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manager {
    #[serde(default)]
    name: String,
    /// Command for adding one/multiple item
    add: String,
    /// Command for adding an item
    remove: String,
    /// Command for getting a whitespace-separated list of all installed items
    list: String,
    /// Command for upgrading all items
    upgrade: Option<String>,

    /// First remove items, then add them
    #[serde(default)]
    remove_then_add: bool,

    /// The separator to use when filling in the <items> in format commands.
    /// Defaults to space
    items_separator: Option<String>,

    /// The items the manager is supposed to have
    #[serde(default)]
    items: HashSet<String>,

    /// The items to add to the system
    #[serde(default)]
    items_to_add: Vec<String>,
    /// The items to remove from the system
    #[serde(default)]
    items_to_remove: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let mut managers = load_managers(cli.managers).context("Failed to load managers")?;
    match cli.command {
        Build | Diff => {
            load_configs(&mut managers).context("Failed to load configs")?;

            compute_add_remove(&mut managers).context("Failed to compute add/remove")?;

            print_diff(&managers);

            if cli.command == Build {
                // If there is anything to do
                if managers.iter().any(|manager| {
                    !manager.items_to_add.is_empty() || !manager.items_to_remove.is_empty()
                }) {
                    // Ask for confirmation
                    if !ask_for_confirmation().context("Failed to ask for confirmation")? {
                        exit(1);
                    };
                    add_remove_items(&managers).context("Failed to add/remove items")?;
                } else {
                    println!("Nothing to do.");
                }
            }
            Ok(())
        }
        Upgrade => upgrade(&managers).context("Failed to upgrade managers"),
    }
}

fn load_managers(managers_to_load: Option<Vec<String>>) -> anyhow::Result<Vec<Manager>> {
    let manager_path = PathBuf::from(format!("{}/managers", *CONFIG_PATH));

    let mut managers = manager_path
        .read_dir()
        .context("Failed to read manager dir")?
        .flatten() // Ignore Err() Results
        // Get manager name & filter out non-toml files
        .filter_map(|file| {
            file.file_name().to_str().and_then(|file_name| {
                file_name
                    .strip_suffix(".toml")
                    .map(|name| (file, name.to_owned()))
            })
        })
        // If --managers is given, only load the given managers
        .filter(
            #[expect(clippy::pattern_type_mismatch)] // Cant seem to get this lint away
            |(_, name)| {
                managers_to_load
                    .as_ref()
                    .is_none_or(|managers| managers.contains(name))
            },
        )
        // Load manager
        .map(|(file, name)| {
            let manager_string = fs::read_to_string(file.path()).with_context(|| {
                format!("Failed to read manager file '{}'", file.path().display())
            })?;
            let mut manager: Manager = toml::from_str(&manager_string)
                .with_context(|| format!("Failed to deserialize manager '{name}'"))?;
            manager.name = name;

            Ok(manager)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let manager_order: Vec<String> = fs::read_to_string(format!("{}/manager_order", *CONFIG_PATH))
        .context("Failed to read manager order")?
        .lines()
        .map(ToOwned::to_owned)
        .collect();

    managers.sort_unstable_by_key(|manager| {
        manager_order
            .iter()
            .position(|ordered_manager| *ordered_manager == manager.name)
    });

    // Assert that all requested managers were found
    if let Some(managers_to_load) = managers_to_load {
        for manager_to_load in managers_to_load {
            if managers
                .iter()
                .any(|manager| manager.name == manager_to_load)
            {
                return Err(anyhow!("Requested Manager not found"));
            }
        }
    }

    Ok(managers)
}

/// Loads the config items for each manager
fn load_configs(managers: &mut [Manager]) -> anyhow::Result<()> {
    // Start at the current machine's config file
    let hostname = fs::read_to_string("/etc/hostname").context("Failed to get hostname")?;
    let hostname = hostname.trim();

    // The list of configs that should be parsed, gets continually extended when a new config file is imported
    // Paths are evaluated relative to CONFIG_PATH/configs/ and are appended with .toml
    let mut configs_to_parse: Vec<String> = vec![format!("../machines/{hostname}")]; // A bit hacky, but should resolve to CONFIG_PATH/machines/{hostname}.toml

    // Cant find a better way that allows pushing while iterating
    let mut i = 0;
    while let Some(config_file) = configs_to_parse.get(i) {
        let config_file = format!("{}/configs/{config_file}.toml", *CONFIG_PATH);

        // Load the config file
        let config_string = fs::read_to_string(config_file)
            .with_context(|| "Failed to read config file '{config_file}'")?;

        // Deserialize it
        let config_table: Table = toml::from_str(&config_string)
            .with_context(|| "Failed to deserialize config '{config_file}'")?;

        for (manager_name, value) in config_table {
            // Create an iterator over the items of the entry
            value
                // Both arrays...
                .as_array()
                .into_iter()
                .flatten()
                // ...and single-value items are allowed
                .chain(value.is_str().then_some(&value))
                .try_for_each(|value| {
                    // Convert item to string
                    let item = value
                        .as_str()
                        .with_context(|| format!("Found non-string item '{value:?}'"))?;

                    // Didnt find a way to push this up without code duplication
                    if manager_name == "imports" {
                        let item = item.to_owned();
                        // Avoid infinite loop when two configs import each other
                        if !configs_to_parse.contains(&item) {
                            configs_to_parse.push(item);
                        }
                    } else {
                        // Add the items to the manager
                        if let Some(manager) = managers
                            .iter_mut()
                            .find(|manager| manager.name == manager_name)
                        {
                            manager.items.insert(item.into());
                        }
                    }

                    Ok::<_, anyhow::Error>(())
                })?;
        }

        i = i.strict_add(1); // i += 1
    }
    Ok(())
}

/// Computes and prints the items to add and remove for each manager
fn compute_add_remove(managers: &mut [Manager]) -> anyhow::Result<()> {
    for manager in managers {
        // Get system items
        let output = Command::new("fish") // TODO: Add setting for which shell to use
            .arg("-c")
            .arg(&manager.list)
            .output()
            .with_context(|| {
                format!(
                    "Failed to execute command 'list' for manager '{}'",
                    manager.name
                )
            })?;

        let system_items = if output.status.success() {
            String::from_utf8(output.stdout)
                .context("Failed to convert command output to String")?
        } else {
            return Err(anyhow!(format!(
                "Command 'list' for manager '{}' failed with stderr: \n{}",
                manager.name,
                String::from_utf8_lossy(&output.stderr)
            )));
        };

        let system_items = system_items
            .split('\n')
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect();

        manager.items_to_add = manager
            .items
            .difference(&system_items)
            .map(Clone::clone)
            .collect();
        manager.items_to_remove = system_items
            .difference(&manager.items)
            .map(Clone::clone)
            .collect();
    }
    Ok(())
}

/// Prints all items to remove/add
fn print_diff(managers: &[Manager]) {
    for manager in managers {
        // If are any items to add/remove
        if !manager.items_to_add.is_empty() | !manager.items_to_remove.is_empty() {
            println!("{}:", manager.name.bold());
            for item_to_add in &manager.items_to_add {
                println!("{}", item_to_add.green());
            }
            for item_to_remove in &manager.items_to_remove {
                println!("{}", item_to_remove.red());
            }
        }
    }
}

/// Asks the user for confirmation. Returns the users answer
fn ask_for_confirmation() -> anyhow::Result<bool> {
    let mut buf = String::new();

    loop {
        buf.clear();

        println!("{}", "Continue?".bold());

        stdin().read_line(&mut buf).context("Failed to get input")?;

        match buf.trim() {
            "y" | "Y" | "yes" | "" => return Ok(true), // newline is defaulted to y
            "n" | "N" | "no" => return Ok(false),
            _ => eprintln!("Please answer with either y or n"),
        }
    }
}

/// Takes a format command (containing <item> or <items>) and runs it with the provided items
fn fmt_run_command(
    format_command: &str,
    items: &[String],
    items_separator: &str,
) -> anyhow::Result<()> {
    // Only add one item at a time
    if format_command.contains("<item>") {
        items
            .iter()
            .map(|item| format_command.replace("<item>", item))
            .try_for_each(run_command)
    // Add all items at once
    } else if format_command.contains("<items>") {
        let items = items.join(items_separator);
        let command = format_command.replace("<items>", &items);
        run_command(command)
    } else {
        Err(anyhow!(
            "Add command should contain either <item> or <items>"
        ))
    }
}

/// Runs the given command using the shell
fn run_command(command: impl AsRef<str>) -> anyhow::Result<()> {
    let command = command.as_ref();

    let status = Command::new("fish")
        .arg("-c")
        .arg(command)
        .status()
        .with_context(|| format!("Failed to spawn child command '{command}'"))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(format!(
            "Command '{command}' did not exit successfully"
        )))
    }
}

/// Adds/removes all items in `to_add`/`to_remove`.
/// Respects `manager_order`
fn add_remove_items(managers: &[Manager]) -> anyhow::Result<()> {
    for manager in managers {
        // Add & remove operations
        let mut operations = [
            (&manager.add, &manager.items_to_add),
            (&manager.remove, &manager.items_to_remove),
        ];
        // Reverse operations if removing should be done first
        if manager.remove_then_add {
            operations.reverse();
        }

        // Run operations
        for (format_command, items) in operations {
            if !items.is_empty() {
                let items_separator = manager.items_separator.as_deref().unwrap_or(" ");
                fmt_run_command(format_command, items, items_separator)
                    .with_context(|| format!("Failed to run fmt command '{format_command}'"))?;
            }
        }
    }
    Ok(())
}

fn upgrade(managers: &[Manager]) -> anyhow::Result<()> {
    for manager in managers {
        if let Some(ref upgrade_command) = manager.upgrade {
            run_command(upgrade_command).with_context(|| {
                format!("Failed to run upgrade command for manager {}", manager.name)
            })?;
        }
    }
    Ok(())
}
