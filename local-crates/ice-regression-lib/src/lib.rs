// This tests our ICE regression handling.

#[cfg(channel_beta)]
fn innocent() {
    break rust;
}

#[cfg(not(channel_beta))]
fn innocent() {
    thisisabuildfailure;
}
