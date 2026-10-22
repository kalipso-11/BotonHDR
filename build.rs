// Embebe el icono del gadget (assets/botonhdr.ico) en el .exe:
// compila el .rc con windres y le pasa el .res al linker.
// (Misma técnica que el crate `winres`, sin agregar la dependencia.)
fn main() {
    #[cfg(windows)]
    {
        let out = std::env::var("OUT_DIR").expect("OUT_DIR");
        let res = format!("{out}\\botonhdr_icon.res");
        let status = std::process::Command::new("windres")
            .arg("assets\\botonhdr.rc")
            .arg("-O")
            .arg("res")
            .arg("-o")
            .arg(&res)
            .arg("-I")
            .arg("assets")
            .status()
            .expect("windres no encontrado en PATH");
        assert!(status.success(), "windres falló al compilar assets/botonhdr.rc");
        println!("cargo:rustc-link-arg={res}");
        println!("cargo:rerun-if-changed=assets/botonhdr.rc");
        println!("cargo:rerun-if-changed=assets/botonhdr.ico");
    }
}
