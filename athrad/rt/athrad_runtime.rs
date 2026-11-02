// Athrad's runtime for the programs its own frontend compiles.
unsafe extern "C-unwind" {
    fn athrad_user_main();
}

fn main() {
    unsafe { athrad_user_main() }
}

fn text<'a>(p: *const u8, n: usize) -> &'a str {
    unsafe { str::from_utf8_unchecked(std::slice::from_raw_parts(p, n)) }
}

fn wide(lo: u64, hi: u64) -> u128 {
    (hi as u128) << 64 | lo as u128
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_str(p: *const u8, n: usize) {
    print!("{}", text(p, n));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_i64(x: i64) {
    print!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_u64(x: u64) {
    print!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_i128(lo: u64, hi: u64) {
    print!("{}", wide(lo, hi) as i128);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_u128(lo: u64, hi: u64) {
    print!("{}", wide(lo, hi));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_print_bool(b: u32) {
    print!("{}", b != 0);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_str(p: *const u8, n: usize) {
    eprint!("{}", text(p, n));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_i64(x: i64) {
    eprint!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_u64(x: u64) {
    eprint!("{x}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_i128(lo: u64, hi: u64) {
    eprint!("{}", wide(lo, hi) as i128);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_u128(lo: u64, hi: u64) {
    eprint!("{}", wide(lo, hi));
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn athrad_rt_eprint_bool(b: u32) {
    eprint!("{}", b != 0);
}
