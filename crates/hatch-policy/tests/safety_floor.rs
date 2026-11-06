//! Safety-floor tests: each of the six classes hits, near misses do
//! not, words used as data are not commands, and a line the lexer
//! cannot read is never clear.

use hatch_policy::{floor_reading, FloorReading, FloorRule};

fn hit(cmd: &str) -> FloorRule {
    hit_home(cmd, Some("/home/moses"))
}

fn hit_home(cmd: &str, home: Option<&str>) -> FloorRule {
    match floor_reading(cmd, home) {
        FloorReading::Hit { rule, label } => {
            assert!(
                !label.contains(cmd),
                "the label must not echo the command: {label}"
            );
            rule
        }
        other => panic!("{cmd:?} should hit the floor, got {other:?}"),
    }
}

fn clear(cmd: &str) {
    assert_eq!(
        floor_reading(cmd, Some("/home/moses")),
        FloorReading::Clear,
        "{cmd:?}"
    );
}

fn unread(cmd: &str) {
    assert_eq!(
        floor_reading(cmd, Some("/home/moses")),
        FloorReading::Unread,
        "{cmd:?}"
    );
}

#[test]
fn delete_root_hits() {
    assert_eq!(hit("rm -rf /"), FloorRule::DeleteRoot);
    assert_eq!(hit("sudo rm -rf /"), FloorRule::DeleteRoot);
    assert_eq!(hit("rm -rf /*"), FloorRule::DeleteRoot);
    assert_eq!(hit("rm -rf / --no-preserve-root"), FloorRule::DeleteRoot);
    assert_eq!(hit("rm -rf /usr/../"), FloorRule::DeleteRoot);
    assert_eq!(hit("rm -rf /"), FloorRule::DeleteRoot);
    let FloorReading::Hit { rule, label } = floor_reading("rm -rf /", None) else {
        panic!("expected a hit");
    };
    assert_eq!(rule, FloorRule::DeleteRoot);
    assert_eq!(rule.as_str(), "delete-root");
    assert_eq!(label, "recursive delete of the filesystem root");
}

#[test]
fn delete_home_hits() {
    assert_eq!(hit("rm -rf ~"), FloorRule::DeleteHome);
    assert_eq!(hit("rm -rf $HOME"), FloorRule::DeleteHome);
    assert_eq!(hit("rm -rf ~/"), FloorRule::DeleteHome);
    assert_eq!(hit("rm -rf /home/moses"), FloorRule::DeleteHome);
    assert_eq!(hit("rm -rf ~/.."), FloorRule::DeleteHome);
    assert_eq!(hit("rm -rf ${HOME}/.."), FloorRule::DeleteHome);
    assert_eq!(hit("rm -rf \"$HOME\""), FloorRule::DeleteHome);
    assert_eq!(
        hit_home("rm -rf /Users/moses", Some("/Users/moses")),
        FloorRule::DeleteHome
    );
}

#[test]
fn delete_system_directory_hits() {
    assert_eq!(hit("rm -rf /etc"), FloorRule::DeleteSystemDirectory);
    assert_eq!(hit("rm -rf /etc/*"), FloorRule::DeleteSystemDirectory);
    assert_eq!(hit("rm -rf /usr"), FloorRule::DeleteSystemDirectory);
    assert_eq!(hit("rm -rf /var"), FloorRule::DeleteSystemDirectory);
    assert_eq!(hit("rm -rf /home"), FloorRule::DeleteSystemDirectory);
    assert_eq!(hit("rm -rf /lib*"), FloorRule::DeleteSystemDirectory);
    assert_eq!(hit("rm -rf /bin"), FloorRule::DeleteSystemDirectory);
}

#[test]
fn format_disk_hits() {
    assert_eq!(hit("mkfs.ext4 /dev/sda1"), FloorRule::FormatDisk);
    assert_eq!(hit("mkfs /dev/nvme0n1p2"), FloorRule::FormatDisk);
    assert_eq!(hit("mkfs -t ext4 /dev/sda"), FloorRule::FormatDisk);
    assert_eq!(
        hit("diskutil erasedisk APFS foo /dev/disk2"),
        FloorRule::FormatDisk
    );
}

#[test]
fn overwrite_disk_hits() {
    assert_eq!(
        hit("dd if=/dev/zero of=/dev/sda bs=1M"),
        FloorRule::OverwriteDisk
    );
    assert_eq!(hit("dd if=x of=/dev/nvme0n1"), FloorRule::OverwriteDisk);
    assert_eq!(hit("echo hi > /dev/sda"), FloorRule::OverwriteDisk);
    assert_eq!(hit("cat > /dev/sdb1"), FloorRule::OverwriteDisk);
}

#[test]
fn fork_bomb_hits() {
    assert_eq!(hit(":(){ :|:& };:"), FloorRule::ForkBomb);
    assert_eq!(hit("foo(){ foo|foo& }; foo"), FloorRule::ForkBomb);
    assert_eq!(
        hit("function bomb { bomb|bomb& }; bomb"),
        FloorRule::ForkBomb
    );
    assert_eq!(
        hit("function bomb() { bomb|bomb& }; bomb"),
        FloorRule::ForkBomb
    );
}

#[test]
fn near_misses_are_clear() {
    // The required near misses: ordinary deletes, relative paths.
    clear("rm -rf ./build");
    clear("rm file.txt");
    clear("rm -r ./node_modules");
    clear("rm -rf /tmp/work");
    clear("rm -rf /etc/passwd");
    clear("rm -rf /var/log");
    clear("rm -rf /opt/app");
    clear("rm -rf ~/projects");
    clear("rm -rf ~moses");
    clear("rm -rf ~/..//projects");
    clear("rm -rf $DIR");
    clear("rm -rf '$HOME'");
    clear("mkfs.ext4 disk.img");
    clear("mkfs.ext4 /dev/loop0");
    clear("dd if=x of=disk.img");
    clear("echo hi > /dev/null");
    clear("cat > /dev/tty");
    clear("echo hi >> /tmp/log");
    clear("ls /");
    clear("ls /etc");
    clear("cat /etc/passwd");
}

#[test]
fn definition_without_invocation_is_not_a_bomb() {
    clear(":(){ :|:& }");
    clear(":(){ :|:& }; echo done");
    clear("echo ':(){ :|:& };:'");
}

#[test]
fn wrappers_are_followed() {
    assert_eq!(hit("sh -c \"rm -rf /\""), FloorRule::DeleteRoot);
    assert_eq!(
        hit("bash -c 'rm -rf /etc'"),
        FloorRule::DeleteSystemDirectory
    );
    assert_eq!(hit("eval \"rm -rf /\""), FloorRule::DeleteRoot);
    assert_eq!(hit("timeout 5 rm -rf /"), FloorRule::DeleteRoot);
    assert_eq!(hit("FOO=1 rm -rf /"), FloorRule::DeleteRoot);
    assert_eq!(hit("env -S \"rm -rf /\""), FloorRule::DeleteRoot);
    assert_eq!(hit("nice rm -rf /"), FloorRule::DeleteRoot);
    assert_eq!(hit("sh -c \"bash -c 'rm -rf /'\""), FloorRule::DeleteRoot);
}

#[test]
fn lookups_and_listings_are_not_commands() {
    clear("sudo -l rm -rf /");
    clear("command -v rm");
    clear("sudo -l");
}

#[test]
fn words_used_as_data_are_not_commands() {
    clear("echo \"rm -rf /\"");
    clear("git commit -m \"rm -rf /\"");
    clear("echo 'rm -rf /' > /tmp/x");
    clear("cat <<'EOF'\nrm -rf /\nEOF");
    clear("echo rm -rf /");
}

#[test]
fn unreadable_lines_are_never_clear() {
    unread("echo \"unterminated");
    unread("ls '/tmp");
    unread("sudo --us root rm -rf /");
    unread(&"a".repeat(33 * 1024));
    clear("");
    clear("   ");
}

#[test]
fn spelled_home_needs_the_home_parameter() {
    clear("rm -rf /Users/moses");
    assert_eq!(
        hit_home("rm -rf /Users/moses", Some("/Users/moses")),
        FloorRule::DeleteHome
    );
    // A home under a temp directory is cleanup, not home.
    clear("rm -rf /tmp/fakehome");
    assert_eq!(
        floor_reading("rm -rf /tmp/fakehome", Some("/tmp/fakehome")),
        FloorReading::Clear
    );
}

#[test]
fn powershell_remove_item_is_judged_too() {
    assert_eq!(
        hit("pwsh -Command \"Remove-Item -Recurse /etc\""),
        FloorRule::DeleteSystemDirectory
    );
    clear("pwsh -Command \"Remove-Item /tmp/work\"");
    clear("pwsh -Command \"Get-ChildItem /etc\"");
}
