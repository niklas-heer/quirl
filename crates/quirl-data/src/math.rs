//! `math sum`, `min`, `max`, and `avg` over typed values.
//!
//! Numbers keep the data runtime's exactness promise: integers and decimal
//! text are added as an `i128` mantissa with a decimal scale, never through
//! binary floating point. A total that needs more than 38 significant digits
//! fails with a resource-limit error instead of losing precision. An average
//! is exact when the division terminates and otherwise rounds half away from
//! zero to [`AVERAGE_FRACTION_DIGITS`] places.
//!
//! Sizes and durations reduce only with values of their own kind, so a total
//! keeps its unit. `min` and `max` accept any mutually comparable values,
//! such as strings or dates, and return the original item.

use crate::syntax::MathOperation;
use crate::{DataValue, compare_values, data_error, limit_error};
use quirl_core::ShellError;
use std::cmp::Ordering;

/// Fractional digits kept when an average does not divide evenly.
const AVERAGE_FRACTION_DIGITS: u32 = 12;
/// Largest decimal scale an exact total may carry.
const SCALE_MAX: u32 = 36;

/// Reduce a list with `operation`.
pub(crate) fn math_values(
    value: DataValue,
    operation: MathOperation,
) -> Result<DataValue, ShellError> {
    let stage = match operation {
        MathOperation::Sum => "math sum",
        MathOperation::Min => "math min",
        MathOperation::Max => "math max",
        MathOperation::Avg => "math avg",
    };
    let DataValue::List(values) = value else {
        return Err(data_error(
            stage,
            format!("{stage} expects a list or stream of values"),
        ));
    };
    match operation {
        MathOperation::Min => extreme(values, Ordering::Less, stage),
        MathOperation::Max => extreme(values, Ordering::Greater, stage),
        MathOperation::Sum => total(&values, stage).map(|total| total.into_value()),
        MathOperation::Avg => {
            if values.is_empty() {
                return Err(data_error(stage, "math avg needs at least one value"));
            }
            let count = u64::try_from(values.len()).unwrap_or(u64::MAX);
            total(&values, stage)?.divide(count, stage)
        }
    }
}

fn extreme(values: Vec<DataValue>, wanted: Ordering, stage: &str) -> Result<DataValue, ShellError> {
    let mut values = values.into_iter();
    let mut best = values
        .next()
        .ok_or_else(|| data_error(stage, format!("{stage} needs at least one value")))?;
    for value in values {
        if compare_values(&value, &best, stage)? == wanted {
            best = value;
        }
    }
    Ok(best)
}

/// A running total that remembers which kind of value it adds.
enum Total {
    Number(Exact),
    Size(u64),
    Duration(u64),
}

impl Total {
    fn into_value(self) -> DataValue {
        match self {
            Self::Number(exact) => exact.into_value(),
            Self::Size(bytes) => DataValue::Size { bytes },
            Self::Duration(nanoseconds) => DataValue::Duration { nanoseconds },
        }
    }

    fn divide(self, count: u64, stage: &str) -> Result<DataValue, ShellError> {
        match self {
            Self::Number(exact) => exact.divide(count, stage).map(Exact::into_value),
            Self::Size(bytes) => Ok(DataValue::Size {
                bytes: rounded_quotient(bytes, count),
            }),
            Self::Duration(nanoseconds) => Ok(DataValue::Duration {
                nanoseconds: rounded_quotient(nanoseconds, count),
            }),
        }
    }
}

/// `total / count` rounded half up; `count` is the non-zero item count.
fn rounded_quotient(total: u64, count: u64) -> u64 {
    let total = u128::from(total);
    let count = u128::from(count.max(1));
    total
        .checked_add(count.checked_div(2).unwrap_or(0))
        .and_then(|rounded| rounded.checked_div(count))
        .and_then(|quotient| u64::try_from(quotient).ok())
        .unwrap_or(u64::MAX)
}

fn total(values: &[DataValue], stage: &str) -> Result<Total, ShellError> {
    let mut total = match values.first() {
        Some(DataValue::Size { .. }) => Total::Size(0),
        Some(DataValue::Duration { .. }) => Total::Duration(0),
        _ => Total::Number(Exact::ZERO),
    };
    for value in values {
        total = match (total, value) {
            (Total::Size(sum), DataValue::Size { bytes }) => {
                Total::Size(sum.checked_add(*bytes).ok_or_else(|| overflow(stage))?)
            }
            (Total::Duration(sum), DataValue::Duration { nanoseconds }) => Total::Duration(
                sum.checked_add(*nanoseconds)
                    .ok_or_else(|| overflow(stage))?,
            ),
            (Total::Number(sum), value) => {
                let Some(value) = Exact::from_value(value, stage)? else {
                    return Err(mixed(stage, value));
                };
                Total::Number(sum.add(value, stage)?)
            }
            (_, value) => return Err(mixed(stage, value)),
        };
    }
    Ok(total)
}

fn mixed(stage: &str, value: &DataValue) -> ShellError {
    data_error(
        stage,
        format!(
            "{stage} adds numbers, sizes, or durations of one kind; found {}",
            crate::value_kind(value)
        ),
    )
}

fn overflow(stage: &str) -> ShellError {
    limit_error(
        format!("{stage} exceeds the exact numeric range"),
        "Reduce fewer or smaller values",
    )
}

/// An exact decimal: `mantissa / 10^scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Exact {
    mantissa: i128,
    scale: u32,
}

impl Exact {
    const ZERO: Self = Self {
        mantissa: 0,
        scale: 0,
    };

    /// Convert a numeric value, or return `None` for any other kind.
    fn from_value(value: &DataValue, stage: &str) -> Result<Option<Self>, ShellError> {
        Ok(match value {
            DataValue::Int(value) => Some(Self {
                mantissa: i128::from(*value),
                scale: 0,
            }),
            DataValue::UInt(value) => Some(Self {
                mantissa: i128::from(*value),
                scale: 0,
            }),
            DataValue::Decimal(text) => Some(Self::parse(text).ok_or_else(|| overflow(stage))?),
            _ => None,
        })
    }

    /// Parse JSON decimal text such as `-1.25` or `3e-2`.
    fn parse(text: &str) -> Option<Self> {
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let (number, exponent) = match unsigned.find(['e', 'E']) {
            Some(index) => (
                unsigned.get(..index)?,
                unsigned
                    .get(index.saturating_add(1)..)?
                    .parse::<i32>()
                    .ok()?,
            ),
            None => (unsigned, 0),
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        let digits = format!("{whole}{fraction}");
        if digits.is_empty()
            || digits.len() > 38
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let mut mantissa = digits.parse::<i128>().ok()?;
        let fraction_digits = i32::try_from(fraction.len()).ok()?;
        let mut scale = fraction_digits.checked_sub(exponent)?;
        if scale < 0 {
            mantissa = mantissa.checked_mul(10_i128.checked_pow(scale.unsigned_abs())?)?;
            scale = 0;
        }
        let scale = u32::try_from(scale)
            .ok()
            .filter(|scale| *scale <= SCALE_MAX)?;
        Some(Self {
            mantissa: if negative {
                mantissa.checked_neg()?
            } else {
                mantissa
            },
            scale,
        })
    }

    fn rescale(self, scale: u32) -> Option<Self> {
        let factor = 10_i128.checked_pow(scale.checked_sub(self.scale)?)?;
        Some(Self {
            mantissa: self.mantissa.checked_mul(factor)?,
            scale,
        })
    }

    fn add(self, other: Self, stage: &str) -> Result<Self, ShellError> {
        let scale = self.scale.max(other.scale);
        let (Some(left), Some(right)) = (self.rescale(scale), other.rescale(scale)) else {
            return Err(overflow(stage));
        };
        let mantissa = left
            .mantissa
            .checked_add(right.mantissa)
            .ok_or_else(|| overflow(stage))?;
        Ok(Self { mantissa, scale })
    }

    fn divide(self, count: u64, stage: &str) -> Result<Self, ShellError> {
        let count = i128::from(count.max(1));
        if self.mantissa.checked_rem(count) == Some(0) {
            return Ok(Self {
                mantissa: self
                    .mantissa
                    .checked_div(count)
                    .ok_or_else(|| overflow(stage))?,
                scale: self.scale,
            });
        }
        let scale = self
            .scale
            .saturating_add(AVERAGE_FRACTION_DIGITS)
            .min(SCALE_MAX);
        let widened = self.rescale(scale).ok_or_else(|| overflow(stage))?;
        let half = count.checked_div(2).unwrap_or(0);
        let adjusted = if widened.mantissa < 0 {
            widened.mantissa.checked_sub(half)
        } else {
            widened.mantissa.checked_add(half)
        }
        .ok_or_else(|| overflow(stage))?;
        Ok(Self {
            mantissa: adjusted.checked_div(count).ok_or_else(|| overflow(stage))?,
            scale,
        }
        .normalized())
    }

    /// Drop trailing fractional zeros.
    fn normalized(mut self) -> Self {
        while self.scale > 0 && self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale = self.scale.saturating_sub(1);
        }
        self
    }

    fn into_value(self) -> DataValue {
        let exact = self.normalized();
        if exact.scale == 0 {
            if let Ok(value) = i64::try_from(exact.mantissa) {
                return DataValue::Int(value);
            }
            if let Ok(value) = u64::try_from(exact.mantissa) {
                return DataValue::UInt(value);
            }
            return DataValue::Decimal(exact.mantissa.to_string());
        }
        let digits = exact.mantissa.unsigned_abs().to_string();
        let scale = usize::try_from(exact.scale).unwrap_or(usize::MAX);
        let padded = format!("{digits:0>width$}", width = scale.saturating_add(1));
        let split = padded.len().saturating_sub(scale);
        let (whole, fraction) = padded.split_at(split);
        let sign = if exact.mantissa < 0 { "-" } else { "" };
        DataValue::Decimal(format!("{sign}{whole}.{fraction}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(values: &[DataValue]) -> DataValue {
        DataValue::List(values.to_vec())
    }

    fn decimal(text: &str) -> DataValue {
        DataValue::Decimal(text.to_owned())
    }

    #[test]
    fn sums_are_exact_across_integers_and_decimals() {
        let values = list(&[
            DataValue::Int(1),
            decimal("0.1"),
            decimal("0.2"),
            DataValue::UInt(2),
        ]);
        assert_eq!(
            math_values(values, MathOperation::Sum).unwrap(),
            decimal("3.3")
        );
        let values = list(&[decimal("0.5"), decimal("0.5")]);
        assert_eq!(
            math_values(values, MathOperation::Sum).unwrap(),
            DataValue::Int(1)
        );
        assert_eq!(
            math_values(list(&[]), MathOperation::Sum).unwrap(),
            DataValue::Int(0)
        );
        let values = list(&[decimal("1e2"), decimal("-2.5E-1")]);
        assert_eq!(
            math_values(values, MathOperation::Sum).unwrap(),
            decimal("99.75")
        );
    }

    #[test]
    fn averages_are_exact_when_division_terminates_and_rounded_otherwise() {
        let values = list(&[DataValue::Int(1), DataValue::Int(2), DataValue::Int(3)]);
        assert_eq!(
            math_values(values, MathOperation::Avg).unwrap(),
            DataValue::Int(2)
        );
        let values = list(&[DataValue::Int(1), DataValue::Int(2)]);
        assert_eq!(
            math_values(values, MathOperation::Avg).unwrap(),
            decimal("1.5")
        );
        let values = list(&[DataValue::Int(1), DataValue::Int(1), DataValue::Int(0)]);
        assert_eq!(
            math_values(values, MathOperation::Avg).unwrap(),
            decimal("0.666666666667")
        );
        let values = list(&[DataValue::Int(-1), DataValue::Int(0), DataValue::Int(0)]);
        assert_eq!(
            math_values(values, MathOperation::Avg).unwrap(),
            decimal("-0.333333333333")
        );
        assert!(math_values(list(&[]), MathOperation::Avg).is_err());
    }

    #[test]
    fn sizes_and_durations_keep_their_unit() {
        let sizes = list(&[
            DataValue::Size { bytes: 1_000 },
            DataValue::Size { bytes: 501 },
        ]);
        assert_eq!(
            math_values(sizes.clone(), MathOperation::Sum).unwrap(),
            DataValue::Size { bytes: 1_501 }
        );
        assert_eq!(
            math_values(sizes, MathOperation::Avg).unwrap(),
            DataValue::Size { bytes: 751 }
        );
        let mixed = list(&[DataValue::Size { bytes: 1 }, DataValue::Int(1)]);
        let error = math_values(mixed, MathOperation::Sum).unwrap_err();
        assert!(error.message.contains("of one kind"), "{}", error.message);
    }

    #[test]
    fn min_and_max_return_the_original_item() {
        let values = list(&[
            DataValue::String("pear".to_owned()),
            DataValue::String("apple".to_owned()),
        ]);
        assert_eq!(
            math_values(values.clone(), MathOperation::Min).unwrap(),
            DataValue::String("apple".to_owned())
        );
        assert_eq!(
            math_values(values, MathOperation::Max).unwrap(),
            DataValue::String("pear".to_owned())
        );
        assert!(math_values(list(&[]), MathOperation::Max).is_err());
    }

    #[test]
    fn totals_beyond_exact_precision_fail_instead_of_rounding() {
        let values = list(&[DataValue::UInt(u64::MAX), DataValue::UInt(u64::MAX)]);
        assert_eq!(
            math_values(values, MathOperation::Sum).unwrap(),
            decimal("36893488147419103230")
        );
        let huge = decimal(&"9".repeat(38));
        let error = math_values(list(&[huge.clone(), huge]), MathOperation::Sum).unwrap_err();
        assert_eq!(error.code, quirl_core::ErrorCode::ResourceLimit);
        assert!(Exact::parse(&"1".repeat(39)).is_none());
    }
}
