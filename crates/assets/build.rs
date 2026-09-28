fn main() {
    // New theme and image files must invalidate the embedded asset list too.
    println!("cargo:rerun-if-changed=../../assets");
}
