fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=data/cycle.gresource.xml");
    println!("cargo:rerun-if-changed=data/thumbnails/");
    println!("cargo:rerun-if-changed=data/icons/activities/");
    println!("cargo:rerun-if-changed=data/icons/symbolic/");

    glib_build_tools::compile_resources(&["data"], "data/cycle.gresource.xml", "cycle.gresource");
}
