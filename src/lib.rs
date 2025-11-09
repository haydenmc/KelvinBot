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
    pub mod echo;
    pub mod invite;
    pub mod logger;
    pub mod regal_showtimes;
}
