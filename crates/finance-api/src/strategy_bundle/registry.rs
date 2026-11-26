use serde::Serialize;
use serde_json::{Value, json};

use super::catalog::Availability;
use super::schema::{
    COMMISSION_BPS, Field, LOT_SIZE_POLICY, Rule, SLIPPAGE_BPS, TextFormat, field,
};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Warmup {
    Fixed { sessions: u64 },
    Parameter { name: &'static str },
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Input {
    pub series: &'static str,
    pub unit: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Output {
    pub kind: &'static str,
    pub unit: &'static str,
}

#[derive(Debug)]
pub struct Component {
    pub id: &'static str,
    pub kind: &'static str,
    pub inputs: &'static [Input],
    pub output: Output,
    pub warmup: Warmup,
    pub position_state: &'static [&'static str],
    pub assets: &'static [&'static str],
    pub engines: &'static [&'static str],
    pub supports_short: bool,
    pub factor_parameters: &'static [&'static str],
    pub availability: Availability,
    pub trigger: Option<&'static str>,
    pub description: &'static str,
    pub(super) parameters: &'static [Field],
}

impl Component {
    pub fn parameter_schema(&self) -> Value {
        Rule::Record(self.parameters).json_schema()
    }

    /// Prior completed sessions. The close at the decision is an additional observation.
    /// Call with structurally validated parameters.
    pub fn warmup_sessions(&self, params: &Value) -> u64 {
        match self.warmup {
            Warmup::Fixed { sessions } => sessions,
            Warmup::Parameter { name } => params[name]
                .as_u64()
                .expect("validated warmup parameter is a bounded nonnegative integer"),
        }
    }

    pub fn declaration(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "parameters": self.parameter_schema(),
            "inputs": self.inputs,
            "output": self.output,
            "warmup": self.warmup,
            "position_state": self.position_state,
            "assets": self.assets,
            "engines": self.engines,
            "supports_short": self.supports_short,
            "factor_parameters": self.factor_parameters,
            "availability": self.availability,
            "trigger": self.trigger,
            "description": self.description,
        })
    }
}

pub(super) const MAX_PERIOD: u64 = 2520;
const PERIOD: Rule = Rule::Integer {
    min: 1,
    max: MAX_PERIOD,
};
const DECIMAL: Rule = Rule::Text(TextFormat::Decimal);
const FACTOR: Rule = Rule::Text(TextFormat::LocalId);
const THRESHOLD_PARAMETERS: &[Field] = &[field("factor", FACTOR), field("threshold", DECIMAL)];
const CLOSE_INPUT: Input = Input {
    series: "daily_bar.close",
    unit: "quote_currency_per_share",
};
const FACTOR_INPUT: Input = Input {
    series: "factor",
    unit: "referenced_factor_output_unit",
};
const POSITION_INPUT: Input = Input {
    series: "filled_position",
    unit: "shares",
};
const TARGET_INPUT: Input = Input {
    series: "portfolio_target",
    unit: "portfolio_weight",
};
const CASH_INPUT: Input = Input {
    series: "available_cash",
    unit: "quote_currency",
};
const TARGET_OUTPUT: Output = Output {
    kind: "target_weights",
    unit: "portfolio_weight",
};

const COMMON: Component = Component {
    id: "",
    kind: "",
    inputs: &[],
    output: Output { kind: "", unit: "" },
    warmup: Warmup::Fixed { sessions: 0 },
    position_state: &[],
    assets: &["cash_equity"],
    engines: &["nautilus_cash_equity"],
    supports_short: false,
    factor_parameters: &[],
    availability: Availability::SessionClose,
    trigger: None,
    description: "",
    parameters: &[],
};

static COMPONENTS: &[Component] = &[
    Component {
        id: "allocator.top_n@1.0.0",
        kind: "allocator",
        inputs: &[FACTOR_INPUT],
        output: TARGET_OUTPUT,
        factor_parameters: &["factor"],
        description: "Rank eligible members by descending FactorValue and allocate equal TargetWeights to up to N members. Break equal scores by stable instrument ID.",
        parameters: &[
            field("factor", FACTOR),
            field("top_n", Rule::Integer { min: 1, max: 1000 }),
        ],
        ..COMMON
    },
    Component {
        id: "entry.above_when_flat@1.0.0",
        kind: "entry",
        inputs: &[FACTOR_INPUT, POSITION_INPUT],
        output: Output {
            kind: "enter_long_intent",
            unit: "none",
        },
        position_state: &["filled_quantity"],
        factor_parameters: &["factor"],
        trigger: Some("level"),
        description: "Enter long when the position is flat and the observed factor is above the threshold.",
        parameters: THRESHOLD_PARAMETERS,
        ..COMMON
    },
    Component {
        id: "entry.below_when_flat@1.0.0",
        kind: "entry",
        inputs: &[FACTOR_INPUT, POSITION_INPUT],
        output: Output {
            kind: "enter_long_intent",
            unit: "none",
        },
        position_state: &["filled_quantity"],
        factor_parameters: &["factor"],
        trigger: Some("level"),
        description: "Enter long when the position is flat and the observed factor is below the threshold.",
        parameters: THRESHOLD_PARAMETERS,
        ..COMMON
    },
    Component {
        id: "execution.next_close_cash_equity@1.0.0",
        kind: "execution",
        inputs: &[
            TARGET_INPUT,
            POSITION_INPUT,
            CASH_INPUT,
            Input {
                series: "next_session.daily_bar.close",
                unit: "quote_currency_per_share",
            },
        ],
        output: Output {
            kind: "simulated_fills",
            unit: "shares",
        },
        position_state: &["filled_quantity"],
        availability: Availability::NextSessionClose,
        description: "Use the next session close for cash-equity simulation with instrument-master lot sizes.",
        parameters: &[LOT_SIZE_POLICY, COMMISSION_BPS, SLIPPAGE_BPS],
        ..COMMON
    },
    Component {
        id: "exit.below_when_long@1.0.0",
        kind: "exit",
        inputs: &[FACTOR_INPUT, POSITION_INPUT],
        output: Output {
            kind: "exit_long_intent",
            unit: "none",
        },
        position_state: &["filled_quantity"],
        factor_parameters: &["factor"],
        trigger: Some("level"),
        description: "Exit a long position when the observed factor falls below the threshold.",
        parameters: THRESHOLD_PARAMETERS,
        ..COMMON
    },
    Component {
        id: "exit.holding_sessions@1.0.0",
        kind: "exit",
        inputs: &[
            POSITION_INPUT,
            Input {
                series: "first_fill_session",
                unit: "session",
            },
        ],
        output: Output {
            kind: "exit_long_intent",
            unit: "none",
        },
        position_state: &["filled_quantity", "first_fill_session"],
        trigger: Some("level"),
        description: "Exit a long position after the configured holding period measured from the first actual fill.",
        parameters: &[
            field("sessions", PERIOD),
            field("start_at", Rule::Literal("first_fill")),
        ],
        ..COMMON
    },
    Component {
        id: "factor.close_return@1.0.0",
        kind: "factor",
        inputs: &[CLOSE_INPUT],
        output: Output {
            kind: "factor_value",
            unit: "return_ratio",
        },
        warmup: Warmup::Parameter {
            name: "lookback_sessions",
        },
        description: "Close-to-close return over the configured number of completed sessions.",
        parameters: &[field("lookback_sessions", PERIOD)],
        ..COMMON
    },
    Component {
        id: "filter.complete_session_bar@1.0.0",
        kind: "filter",
        inputs: &[
            Input {
                series: "daily_bar",
                unit: "ohlcv",
            },
            Input {
                series: "daily_bar.volume",
                unit: "shares",
            },
        ],
        output: Output {
            kind: "eligible_for_new_risk",
            unit: "boolean",
        },
        description: "Require a complete session bar and optionally positive volume before adding risk.",
        parameters: &[field("require_positive_volume", Rule::Boolean)],
        ..COMMON
    },
    Component {
        id: "indicator.rsi_wilder@1.0.0",
        kind: "indicator",
        inputs: &[CLOSE_INPUT],
        output: Output {
            kind: "factor_value",
            unit: "rsi_points_0_to_100",
        },
        warmup: Warmup::Parameter { name: "period" },
        description: "Wilder RSI uses the close. An undefined flat-series value is missing.",
        parameters: &[
            field(
                "period",
                Rule::Integer {
                    min: 2,
                    max: MAX_PERIOD,
                },
            ),
            field("field", Rule::Literal("close")),
        ],
        ..COMMON
    },
    Component {
        id: "risk.long_only_cash@1.0.0",
        kind: "risk",
        inputs: &[TARGET_INPUT, POSITION_INPUT, CASH_INPUT],
        output: Output {
            kind: "risk_limited_targets",
            unit: "portfolio_weight",
        },
        position_state: &["filled_quantity"],
        description: "Restrict targets to nonnegative cash-equity positions within the gross cap, without borrowing.",
        parameters: &[
            field("max_gross_weight", DECIMAL),
            field("allow_borrow", Rule::Boolean),
        ],
        ..COMMON
    },
    Component {
        id: "sizing.equal_weight_capped@1.0.0",
        kind: "sizing",
        inputs: &[Input {
            series: "eligible_entry_intents",
            unit: "none",
        }],
        output: TARGET_OUTPUT,
        description: "Equal weight eligible entries within the sleeve budget and per-asset cap.",
        parameters: &[field("max_weight_per_asset", DECIMAL)],
        ..COMMON
    },
];

pub fn components() -> &'static [Component] {
    COMPONENTS
}

pub fn component(id: &str) -> Option<&'static Component> {
    COMPONENTS.iter().find(|item| item.id == id)
}

pub(super) fn component_registry() -> Value {
    let declarations: Vec<_> = components().iter().map(Component::declaration).collect();
    json!({
        "apiVersion": "fertile.finance/strategy/v1",
        "kind": "ComponentRegistry",
        "components": declarations,
    })
}
