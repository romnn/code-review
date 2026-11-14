/// Computes a simple position-weighted checksum of the entries' amounts.
pub fn checksum(amounts: &[i64]) -> i64 {
    let mut sum = 0i64;
    for (index, amount) in amounts.iter().enumerate() {
        let weight = i64::try_from(index).unwrap_or(i64::MAX).wrapping_add(1);
        sum = sum.wrapping_add(amount.wrapping_mul(weight));
    }
    sum
}
