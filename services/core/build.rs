fn main() {
    #[cfg(feature = "ui")]
    slint_build::compile("ui/appwindow.slint").expect("Failed to compile Slint UI definitions");
}
