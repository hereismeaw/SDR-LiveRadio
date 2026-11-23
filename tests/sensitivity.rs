//! Pilot-coherent RDS carrier recovery must not be worse than BPSK squaring.
//! (On the synthetic signal both reach the same block rate: the carrier is stable,
//! so bit decisions, not carrier tracking, limit sensitivity.)

#[test]
fn coherent_carrier_is_not_worse_than_squaring() {
    let mut report = String::new();
    for (i, noise) in [0.10f32, 0.20, 0.30].into_iter().enumerate() {
        let (coh, sq) = rds_fm::synth::rds_block_rate(noise, 0xC0FFEE + i as u64);
        report += &format!("noise {noise:.2}: coherent {:.0}%  squaring {:.0}%\n", coh * 100.0, sq * 100.0);
        assert!(coh + 0.05 >= sq, "coherent clearly worse than squaring\n{report}");
    }
    let (coh, _) = rds_fm::synth::rds_block_rate(0.10, 1);
    assert!(coh > 0.95, "clean signal must decode\n{report}");
    println!("{report}");
}
