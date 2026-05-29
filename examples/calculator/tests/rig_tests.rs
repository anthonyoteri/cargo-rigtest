use std::sync::Arc;

use rigtest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct State {
    max_operand: i64,
}

#[global_setup]
async fn setup() -> State {
    State {
        max_operand: 1_000_000,
    }
}

#[global_teardown]
async fn teardown(_state: State) {}

#[testcase]
async fn add_positive_numbers(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::add(2, 2), 4);
    assert_eq!(calculator::add(100, 200), 300);
    Ok(())
}

#[testcase]
async fn add_negative_numbers(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::add(-3, -7), -10);
    assert_eq!(calculator::add(-1, 1), 0);
    Ok(())
}

#[testcase]
async fn large_number_arithmetic_skipped_on_32bit(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if std::mem::size_of::<usize>() < 8 {
        rigtest::skip!("requires 64-bit platform");
    }
    assert_eq!(calculator::add(i64::MAX, 0), i64::MAX);
    Ok(())
}

#[testcase]
async fn subtract_positive_numbers(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::subtract(10, 3), 7);
    assert_eq!(calculator::subtract(0, 5), -5);
    Ok(())
}

#[testcase]
async fn multiply_negative_numbers(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::multiply(-2, 5), -10);
    assert_eq!(calculator::multiply(-3, -3), 9);
    Ok(())
}

#[testcase]
async fn divide_by_zero_returns_none(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::divide(42, 0), None);
    Ok(())
}

#[testcase(timeout = std::time::Duration::from_secs(5))]
async fn completes_within_timeout(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::add(1, 1), 2);
    Ok(())
}

#[testcase(serial)]
async fn exclusive_resource_access(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(calculator::multiply(6, 7), 42);
    Ok(())
}

#[testcase]
async fn respects_configured_max_operand(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = ctx.global_data.downcast_ref::<State>().unwrap();
    assert_eq!(calculator::add(cfg.max_operand, 1), cfg.max_operand + 1);
    Ok(())
}

#[testcase]
async fn stateful_calculator_records_history(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut calc = ctx
        .setup(|global| async move {
            let _cfg = global.downcast_ref::<State>().unwrap();
            Ok(calculator::Calculator::new())
        })
        .await?;

    calc.add(2, 3);
    calc.add(10, 20);
    assert_eq!(calc.history().len(), 2);
    assert_eq!(calc.history()[0], "2 + 3 = 5");
    assert_eq!(calc.history()[1], "10 + 20 = 30");

    ctx.teardown(|global| async move {
        let cfg = global.downcast_ref::<State>().unwrap();
        for entry in calc.history() {
            if entry.contains("= -") && cfg.max_operand > 0 {
                return Err(format!("unexpected negative result: {entry}").into());
            }
        }
        Ok(())
    })
    .await?;

    Ok(())
}

#[testcase]
async fn stateful_calculator_handles_division(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut calc = ctx
        .setup(|_global| async {
            println!("setting up calculator for division test");
            Ok(calculator::Calculator::new())
        })
        .await?;

    assert_eq!(calc.divide(10, 2), Some(5));
    assert_eq!(calc.divide(7, 0), None);

    assert_eq!(calc.history()[0], "10 / 2 = Some(5)");
    assert_eq!(calc.history()[1], "7 / 0 = None");

    ctx.teardown(|_global| async move {
        drop(calc);
        Ok(())
    })
    .await?;

    Ok(())
}

fn main() {
    rigtest::run_main();
}
