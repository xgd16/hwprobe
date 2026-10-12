//! 数据模型:类别、设备、传感器、读值、状态码。
//!
//! 整体结构为 HWiNFO 风格的三层树:Category → Device → Sensor。
//! 传感器寻址使用 (category, device_idx, sensor_idx) 三元键,稳定且便于 C ABI 编组。

use serde_json::json;

pub const ALL_CATEGORIES: u32 = u32::MAX;

/// 硬件类别。repr(u32) 保证与 C ABI 中的取值一致。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Cpu = 0,
    Memory = 1,
    Gpu = 2,
    Disk = 3,
    Network = 4,
    Battery = 5,
    Motherboard = 6,
    /// 显示器/面板(掌机内屏):分辨率/刷新率/连接类型/触屏。
    Display = 7,
    /// 惯性传感器(加速度计/陀螺仪,Windows Sensor API)。
    Imu = 8,
}

pub const CATEGORY_COUNT: u32 = 9;

impl Category {
    pub const ALL: [Category; CATEGORY_COUNT as usize] = [
        Category::Cpu,
        Category::Memory,
        Category::Gpu,
        Category::Disk,
        Category::Network,
        Category::Battery,
        Category::Motherboard,
        Category::Display,
        Category::Imu,
    ];

    pub fn from_u32(v: u32) -> Option<Category> {
        match v {
            0 => Some(Category::Cpu),
            1 => Some(Category::Memory),
            2 => Some(Category::Gpu),
            3 => Some(Category::Disk),
            4 => Some(Category::Network),
            5 => Some(Category::Battery),
            6 => Some(Category::Motherboard),
            7 => Some(Category::Display),
            8 => Some(Category::Imu),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Category::Cpu => "CPU",
            Category::Memory => "Memory",
            Category::Gpu => "GPU",
            Category::Disk => "Disk",
            Category::Network => "Network",
            Category::Battery => "Battery",
            Category::Motherboard => "Motherboard",
            Category::Display => "Display",
            Category::Imu => "IMU",
        }
    }
}

/// API 级状态码,同时是每个导出函数的返回值。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    /// 动态传感器还没等到第一次刷新(瞬时状态)
    Waiting = 1,
    NotInitialized = 2,
    InitFailed = 3,
    InvalidArg = 4,
    BadCategory = 5,
    BadDevice = 6,
    BadSensor = 7,
    /// 需要内核驱动(温度/风扇/电压),驱动不可用
    NoDriver = 8,
    NotSupported = 9,
    Error = 10,
    BufferTooSmall = 11,
    Unknown = 12,
    /// 订阅句柄不存在(或已取消)
    BadHandle = 13,
    /// 权限不足(如 macOS 上 NVMe SMART、部分 SMC 键需要 root)
    NoPermission = 14,
}

impl Status {
    pub fn from_u32(v: u32) -> Status {
        match v {
            0 => Status::Ok,
            1 => Status::Waiting,
            2 => Status::NotInitialized,
            3 => Status::InitFailed,
            4 => Status::InvalidArg,
            5 => Status::BadCategory,
            6 => Status::BadDevice,
            7 => Status::BadSensor,
            8 => Status::NoDriver,
            9 => Status::NotSupported,
            10 => Status::Error,
            11 => Status::BufferTooSmall,
            13 => Status::BadHandle,
            14 => Status::NoPermission,
            _ => Status::Unknown,
        }
    }

    pub fn is_ok(self) -> bool {
        self == Status::Ok
    }
}

/// 传感器类型:静态(识别信息,probe 时取一次)或动态(轮询刷新)。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    Static = 0,
    Dynamic = 1,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    F64 = 0,
    U64 = 1,
    I64 = 2,
    Bool = 3,
    Str = 4,
}

#[derive(Debug, Clone)]
pub enum Value {
    F64(f64),
    U64(u64),
    I64(i64),
    Bool(bool),
    Str(String),
}

impl Value {
    pub fn type_tag(&self) -> ValueType {
        match self {
            Value::F64(_) => ValueType::F64,
            Value::U64(_) => ValueType::U64,
            Value::I64(_) => ValueType::I64,
            Value::Bool(_) => ValueType::Bool,
            Value::Str(_) => ValueType::Str,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::F64(v) => {
                if v.is_finite() {
                    json!(v)
                } else {
                    serde_json::Value::Null
                }
            }
            Value::U64(v) => json!(v),
            Value::I64(v) => json!(v),
            Value::Bool(v) => json!(v),
            Value::Str(v) => json!(v),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(v) => Some(*v),
            Value::U64(v) => Some(*v as f64),
            Value::I64(v) => Some(*v as f64),
            _ => None,
        }
    }
}

/// 单次读值:数值 + 精确状态 + 时间戳。"靠谱"的体现之一:
/// 每个传感器独立携带状态,单个数据源故障不影响其他字段。
#[derive(Debug, Clone)]
pub struct Reading {
    pub value: Value,
    pub status: Status,
    pub timestamp_ms: u64,
}

impl Reading {
    pub fn now(value: Value) -> Reading {
        Reading {
            value,
            status: Status::Ok,
            timestamp_ms: crate::util::now_ms(),
        }
    }

    pub fn waiting(vtype: ValueType) -> Reading {
        Reading {
            value: Value::F64(0.0),
            status: Status::Waiting,
            timestamp_ms: crate::util::now_ms(),
        }
        .with_type(vtype)
    }

    pub fn failed(status: Status, vtype: ValueType) -> Reading {
        Reading {
            value: Value::F64(0.0),
            status,
            timestamp_ms: crate::util::now_ms(),
        }
        .with_type(vtype)
    }

    fn with_type(mut self, vtype: ValueType) -> Reading {
        self.value = match vtype {
            ValueType::F64 => Value::F64(0.0),
            ValueType::U64 => Value::U64(0),
            ValueType::I64 => Value::I64(0),
            ValueType::Bool => Value::Bool(false),
            ValueType::Str => Value::Str(String::new()),
        };
        self
    }
}

/// 传感器描述。`code` 是 provider 内部稳定键(如 "load_total"),
/// 刷新逻辑按 code 写值,避免依赖易错的数组下标。
#[derive(Debug, Clone)]
pub struct SensorDesc {
    pub code: String,
    pub name: String,
    pub unit: String,
    pub kind: SensorKind,
    pub vtype: ValueType,
}

#[derive(Debug, Clone)]
pub struct SensorData {
    pub desc: SensorDesc,
    pub current: Reading,
}

impl SensorData {
    pub fn find(sensors: &[SensorData], code: &str) -> Option<usize> {
        sensors.iter().position(|s| s.desc.code == code)
    }
}

/// 设备元信息(静态识别)。id 是跨启动稳定的实例标识
/// (PNP 路径 / PCI 地址 / NVML UUID / 适配器 GUID)。
#[derive(Debug, Clone, Default)]
pub struct DeviceMeta {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub serial: String,
}

#[derive(Debug, Clone)]
pub struct DeviceData {
    pub meta: DeviceMeta,
    pub sensors: Vec<SensorData>,
}

impl DeviceData {
    pub fn set_reading(&mut self, code: &str, reading: Reading) {
        if let Some(i) = SensorData::find(&self.sensors, code) {
            self.sensors[i].current = reading;
        }
    }
}

/// provider 构建设备用的 builder,让每个 provider 的代码聚焦在"取数据"上。
pub struct DeviceBuilder {
    meta: DeviceMeta,
    sensors: Vec<SensorData>,
}

impl DeviceBuilder {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> DeviceBuilder {
        DeviceBuilder {
            meta: DeviceMeta {
                id: id.into(),
                name: name.into(),
                ..Default::default()
            },
            sensors: Vec::new(),
        }
    }

    pub fn vendor(mut self, v: impl Into<String>) -> Self {
        self.meta.vendor = v.into();
        self
    }

    pub fn model(mut self, v: impl Into<String>) -> Self {
        self.meta.model = v.into();
        self
    }

    pub fn serial(mut self, v: impl Into<String>) -> Self {
        self.meta.serial = v.into();
        self
    }

    pub fn static_f64(mut self, code: &str, name: &str, unit: &str, v: f64) -> Self {
        self.sensors.push(SensorData {
            desc: SensorDesc {
                code: code.into(),
                name: name.into(),
                unit: unit.into(),
                kind: SensorKind::Static,
                vtype: ValueType::F64,
            },
            current: Reading::now(Value::F64(v)),
        });
        self
    }

    pub fn static_u64(mut self, code: &str, name: &str, unit: &str, v: u64) -> Self {
        self.sensors.push(SensorData {
            desc: SensorDesc {
                code: code.into(),
                name: name.into(),
                unit: unit.into(),
                kind: SensorKind::Static,
                vtype: ValueType::U64,
            },
            current: Reading::now(Value::U64(v)),
        });
        self
    }

    pub fn static_str(mut self, code: &str, name: &str, v: impl Into<String>) -> Self {
        self.sensors.push(SensorData {
            desc: SensorDesc {
                code: code.into(),
                name: name.into(),
                unit: String::new(),
                kind: SensorKind::Static,
                vtype: ValueType::Str,
            },
            current: Reading::now(Value::Str(v.into())),
        });
        self
    }

    /// 动态传感器:声明后由 refresh 按 code 写值,初值为 Waiting。
    pub fn dynamic(mut self, code: &str, name: &str, unit: &str, vtype: ValueType) -> Self {
        self.sensors.push(SensorData {
            desc: SensorDesc {
                code: code.into(),
                name: name.into(),
                unit: unit.into(),
                kind: SensorKind::Dynamic,
                vtype,
            },
            current: Reading::waiting(vtype),
        });
        self
    }

    /// 动态传感器,但 probe 时就能给出首值(例如静态来源 + 动态来源混合)。
    pub fn dynamic_now(mut self, code: &str, name: &str, unit: &str, value: Value) -> Self {
        self.sensors.push(SensorData {
            desc: SensorDesc {
                code: code.into(),
                name: name.into(),
                unit: unit.into(),
                kind: SensorKind::Dynamic,
                vtype: value.type_tag(),
            },
            current: Reading::now(value),
        });
        self
    }

    pub fn build(self) -> DeviceData {
        DeviceData {
            meta: self.meta,
            sensors: self.sensors,
        }
    }
}
