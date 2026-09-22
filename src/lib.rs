// rustc 1.94+ overflows the default recursion limit while proving `Send` for the
// matrix-sdk sync future spawned in `services::matrix`. The overflow is evaluated in
// this crate, so it is needed regardless of the SDK version (still required on 0.19).
// Upstream applied the same attribute to their own crate in matrix-sdk 0.17
// (matrix-org/matrix-rust-sdk#6254, #6489).
#![recursion_limit = "256"]

pub mod store;

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
    pub mod mumble;
}

pub mod middlewares {
    pub mod attendance_relay;
    pub mod calendar_agenda;
    pub mod chat_relay;
    pub mod echo;
    pub mod ezstream_announce;
    pub mod kanidm;
    pub mod logger;
    pub mod lychee_upload;
    pub mod movie_showtimes;
    pub mod weekly_gathering;
}
