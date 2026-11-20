use super::*;

fn usage(arg: &str) -> String {
    Variant::parse(arg).unwrap_err().to_string()
}

#[test]
fn a_variant_sets_the_three_constants_and_keeps_the_rest() {
    let v = Variant::parse("greedy=40,128,10").unwrap();
    let d = PlanConfig::default();
    assert_eq!(v.name, "greedy");
    assert_eq!(v.config.whole_cov_percent, 40);
    assert_eq!(v.config.small_pack_bytes, 128 << 20);
    assert_eq!(v.config.small_whole_cov_percent, 10);
    assert_eq!(v.config.cold, d.cold);
    assert_eq!(v.config.backlog_ms, d.backlog_ms);
}

#[test]
fn the_design_variant_is_the_defaults_and_the_design_arguments_make_it_too() {
    let design = Variant::design();
    assert_eq!(design.name, "design");
    assert_eq!(design.config, PlanConfig::default());
    assert_eq!(
        Variant::parse("design=50,64,20").unwrap().config,
        design.config
    );
}

#[test]
fn a_variant_that_is_not_name_and_three_numbers_is_refused_naming_it() {
    assert_eq!(
        usage("x"),
        "--variant \"x\": expected NAME=WHOLE_COV,SMALL_MIB,SMALL_COV"
    );
    assert_eq!(
        usage("=1,2,3"),
        "--variant \"=1,2,3\": expected NAME=WHOLE_COV,SMALL_MIB,SMALL_COV"
    );
    assert_eq!(
        usage("x=1,2"),
        "--variant \"x=1,2\": expected three numbers after the name"
    );
    assert_eq!(usage("x=a,2,3"), "--variant \"x=a,2,3\": not a number");
    assert_eq!(usage("x=1,b,3"), "--variant \"x=1,b,3\": not a number");
    assert_eq!(usage("x=1,2,c"), "--variant \"x=1,2,c\": not a number");
    assert_eq!(
        usage("x=101,2,3"),
        "--variant \"x=101,2,3\": a coverage is from 0 to 100"
    );
    assert_eq!(
        usage("x=300,2,3"),
        "--variant \"x=300,2,3\": a coverage is from 0 to 100"
    );
    assert_eq!(
        usage("x=1,2,101"),
        "--variant \"x=1,2,101\": a coverage is from 0 to 100"
    );
}

#[test]
fn a_trees_file_is_one_tree_per_line_without_blanks() {
    assert_eq!(parse_trees("a\n\n  b  \r\n\nc"), ["a", "b", "c"]);
    assert!(parse_trees("\n \n").is_empty());
}
