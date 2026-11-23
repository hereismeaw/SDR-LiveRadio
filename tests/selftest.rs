#[test]
fn synthetic_two_station_end_to_end() {
    match rds_fm::synth::selftest() {
        Ok(report) => println!("{report}"),
        Err(e) => panic!("{e}"),
    }
}
