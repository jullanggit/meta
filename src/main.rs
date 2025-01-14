#![feature(strict_overflow_ops)]

mod cli;

use clap::Parser as _;
use cli::{
    Cli,
    Commands::{Build, Diff, Upgrade},
};
use colored::Colorize as _;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsStr,
    fs,
    io::stdin,
    path::PathBuf,
    process::{Command, exit},
    sync::LazyLock,
};
use toml::Table;

static CONFIG_PATH: LazyLock<String> = LazyLock::new(|| {
    let home = env::var("HOME").expect("HOME is not set");
    format!("{home}/.config/meta")
});

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manager {
    /// Command for adding one/multiple item
    add: String,
    /// Command for adding an item
    remove: String,
    /// Command for getting a whitespace-separated list of all installed items
    list: String,
    /// Command for upgrading all items
    upgrade: Option<String>,

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

fn main() {
    let cli = Cli::parse();

    let mut managers = load_managers();
    match cli.command {
        Build | Diff => {
            load_configs(&mut managers);

            compute_add_remove(&mut managers);

            print_diff(&managers);

            if cli.command == Build {
                if !ask_for_confirmation() {
                    return;
                };
                add_remove_items(&managers);
            }
        }
        Upgrade => upgrade(&managers),
    }
}

fn load_managers() -> HashMap<String, Manager> {
    let manager_path = PathBuf::from(format!("{}/managers", *CONFIG_PATH));

    manager_path
        .read_dir()
        .expect("Failed to read manager dir")
        .flatten() // Ignore Err() Results
        .filter(|file| file.path().extension() == Some(OsStr::new("toml")))
        .map(|manager_file| {
            let manager_string =
                fs::read_to_string(manager_file.path()).expect("Failed to read manager file");
            let manager: Manager =
                toml::from_str(&manager_string).expect("Failed to deserialize manager");

            let name = manager_file
                .file_name()
                .to_str()
                .expect("Failed to get manager name")
                .strip_suffix(".toml")
                .expect("File should be a toml")
                .into();

            (name, manager)
        })
        .collect()
}

/// Loads the config items for each manager
fn load_configs(managers: &mut HashMap<String, Manager>) {
    // Start at the current machine's config file
    let hostname = fs::read_to_string("/etc/hostname").expect("Failed to get hostname");
    let hostname = hostname.trim();

    // The list of configs that should be parsed, gets continually extended when a new config file is imported
    // Paths are evaluated relative to CONFIG_PATH/configs/ and are appended with .toml
    let mut configs_to_parse: Vec<String> = vec![format!("../machines/{hostname}")]; // A bit hacky, but should resolve to CONFIG_PATH/machines/{hostname}.toml

    // Cant find a better way that allows pushing while iterating
    let mut i = 0;
    while let Some(config_file) = configs_to_parse.get(i) {
        let config_file = format!("{}/configs/{config_file}.toml", *CONFIG_PATH);

        // Load the config file
        let config_string = fs::read_to_string(config_file).expect("Config file should exist");

        // Deserialize it
        let config_table: Table =
            toml::from_str(&config_string).expect("Failed to deserialize config");

        for (manager_name, value) in config_table {
            // Create an iterator over the items of the entry
            value
                // Both arrays...
                .as_array()
                .into_iter()
                .flat_map(|vec| {
                    vec.iter()
                        .map(|value| value.as_str().expect("Item should be a string"))
                })
                // ...and single-value items are allowed
                .chain(value.as_str().into_iter())
                .for_each(|item| {
                    // Didnt find a way to push this up without code duplication
                    if manager_name == "imports" {
                        let item = item.into();
                        // Avoid infinite loop when two configs import each other
                        if !configs_to_parse.contains(&item) {
                            configs_to_parse.push(item);
                        }
                    } else {
                        // Add the items to the manager
                        managers
                            .get_mut(&manager_name)
                            .expect("Manager should exist")
                            .items
                            .insert(item.into());
                    }
                });
        }

        i = i.strict_add(1); // i += 1
    }
}

/// Computes and prints the items to add and remove for each manager
fn compute_add_remove(managers: &mut HashMap<String, Manager>) {
    for (manager_name, manager) in managers {
        // Get system items
        let output = Command::new("fish").arg("-c").arg(&manager.list).output(); // TODO: Add setting for which shell to use

        let system_items = match output {
            Ok(output) => {
                if output.status.success() {
                    String::from_utf8(output.stdout).expect("Command output should be UTF-8")
                } else {
                    eprintln!(
                        "Command 'list' for manager {manager_name} failed with error: \n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    exit(1);
                }
            }
            Err(e) => {
                eprintln!("Failed to execute command 'list': {e}");
                exit(1);
            }
        };

        let system_items = system_items
            .split_whitespace()
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
}

/// Prints all items to remove/add
fn print_diff(managers: &HashMap<String, Manager>) {
    for (manager_name, manager) in managers {
        // If are any items to add/remove
        if !manager.items_to_add.is_empty() | !manager.items_to_remove.is_empty() {
            println!("{}:", manager_name.bold());
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
fn ask_for_confirmation() -> bool {
    println!("{}", "Continue?".bold());

    let mut buf = String::new();

    stdin().read_line(&mut buf).expect("Failed to get input");

    match buf.trim() {
        "y" | "Y" | "" => true, // newline is defaulted to y
        _ => false,
    }
}

/// Takes a formatted command (containing <item> or <items>) and runs it with the provided items
fn fmt_run_command(format_command: &str, items: &[String]) {
    // Only add one item at a time
    if format_command.contains("<item>") {
        items
            .iter()
            .map(|item| format_command.replace("<item>", item))
            .for_each(run_command);
    // Add all items at once
    } else if format_command.contains("<items>") {
        let items = items.join(" "); // TODO: Maybe make the separator configurable
        let command = format_command.replace("<items>", &items);
        run_command(command);
    } else {
        eprintln!("Add command should contain either <item> or <items>");
        exit(1);
    };
}

/// Runs the given command using the shell
fn run_command(command: String) {
    let status = Command::new("fish")
        .arg("-c")
        .arg(command)
        .status()
        .expect("Failed to spawn child");

    if !status.success() {
        eprintln!("Command did not exit successfully");
        exit(1);
    }
}

/// Adds/removes all items in `to_add`/`to_remove`.
/// Respects `manager_order`
fn add_remove_items(managers: &HashMap<String, Manager>) {
    let ordered_managers = ordered_managers(managers);

    for manager in ordered_managers {
        // Add new items
        fmt_run_command(&manager.add, &manager.items_to_add);
        // Remove old items
        fmt_run_command(&manager.remove, &manager.items_to_remove);
    }
}

/// Returns the managers in the order specified in `manager_order`
fn ordered_managers(managers: &HashMap<String, Manager>) -> Vec<&Manager> {
    let manager_order = fs::read_to_string(format!("{}/manager_order", *CONFIG_PATH))
        .expect("Failed to read manager order");
    let ordered_managers = manager_order.lines();

    if !ordered_managers.clone().count() == managers.len() {
        eprintln!("Manager missing from manager_order"); // TODO: Maybe report which one
        exit(1);
    }

    ordered_managers
        .map(|manager_name| managers.get(manager_name).expect("Failed to get manager"))
        .collect()
}

fn upgrade(managers: &HashMap<String, Manager>) {
    let ordered_managers = ordered_managers(managers);

    for manager in ordered_managers {
        if let Some(upgrade_command) = manager.upgrade.clone() {
            run_command(upgrade_command);
        }
    }
}
