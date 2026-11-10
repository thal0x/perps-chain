use consensus::sim::*;

#[test]
fn four_nodes_happy_path() {
    let mut sim = Sim::new(SimConfig::new(4, 1));
    sim.run_for(10_000);
    sim.check_safety().unwrap();
    let h = sim.max_honest_committed();
    println!("height after 10s: {h}, min {}", sim.min_honest_committed());
    for i in 0..4 { println!("node {i}: {:?}", sim.core(i).unwrap().stats()); }
    assert!(h >= 30, "height {h}");
    assert!(sim.min_honest_committed() + 3 >= h);
    assert_eq!(sim.total_divergences(), 0);
}
