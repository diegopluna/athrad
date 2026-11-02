// Inline assembly: Athrad rejects it (it has no assembler).
fn main() {
    unsafe { std::arch::asm!("nop") };
    println!("after a nop");
}
