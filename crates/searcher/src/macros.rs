/// 类似assert_eq，但是对于长字符串输出更好。
#[cfg(test)]
#[macro_export]
macro_rules! assert_eq_printed {
    ($expected:expr, $got:expr, $($tt:tt)*) => {
        let expected = &*$expected;
        let got = &*$got;
        let label = format!($($tt)*);
        if expected != got {
            panic!("
printed outputs differ! (label: {})

expected:
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
{}
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

got:
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
{}
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
", label, expected, got);
        }
    }
}
