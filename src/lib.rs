pub mod core {
    pub mod bus;
    pub mod config;
    pub mod event;
    pub mod middleware;
    pub mod service;
}

pub mod services {
    pub mod dummy;
    pub mod matrix;
}

pub mod middlewares {
    pub mod logger;
}
