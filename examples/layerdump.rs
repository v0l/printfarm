fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = std::path::PathBuf::from(&a[1]);
    let want: f32 = a[2].parse().unwrap();
    let tp = printfarm::toolpath::parse(&path).unwrap();
    let mut out = String::new();
    for s in tp.segs.iter().filter(|s| (s.z - want).abs() < 0.001) {
        out.push_str(&format!("S {} {} {} {}\n", s.a[0], s.a[1], s.b[0], s.b[1]));
    }
    let built = printfarm::toolpath::build(&tp);
    for (surface, &l) in built.scene.surfaces.iter().zip(&built.layer_of) {
        if (tp.layer_z[l] - want).abs() < 0.001 {
            for t in surface.positions.chunks(3) {
                out.push_str(&format!(
                    "T {} {} {} {} {} {}\n",
                    t[0][0], t[0][1], t[1][0], t[1][1], t[2][0], t[2][1]
                ));
            }
        }
    }
    std::fs::write(&a[3], out).unwrap();
}
