// Desktop builds of the phone app are for testing it against a mesh.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    patchbay_remote_lib::run()
}
