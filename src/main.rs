#[macro_use]
extern crate log;

use crate::input::DataInput;
use std::process::exit;
use std::sync::Arc;

mod database;
mod input;
mod json;
mod wiki_data_line;
mod wiki_sparql;
mod wiki_time;

fn main() {
    fern::Dispatch::new()
        .format(move |out, msg, record| {
            out.finish(format_args!(
                "[{} {}] {}",
                record.level(),
                record.target(),
                msg
            ))
        })
        .level(log::LevelFilter::Debug)
        .chain(std::io::stdout())
        .apply()
        .unwrap();

    let url = "https://dumps.wikimedia.org/wikidatawiki/entities/latest-all.json.bz2";

    let db_writer = {
        let data_input = input::http::HttpBz2DataInput::new(url.into());
        // let data_input = input::file::Bz2FileInput::new(std::fs::File::open(file).unwrap());
        let mut lines = input::InputLineIter::new(data_input);

        let classes = Arc::new(match wiki_sparql::Classes::new_from_http() {
            Ok(classes) => classes,
            Err(e) => {
                error!("Failed to fetch classes: {}", e);
                exit(-1);
            }
        });

        let (send, recv) = crossbeam::channel::unbounded();

        let db_writer = std::thread::spawn(move || match database::db_writer(recv) {
            Ok(()) => (),
            Err(e) => {
                error!("database writer exited with error: {}", e);
                exit(-1);
            }
        });

        let (cancel_send, cancel_recv) = crossbeam::channel::bounded(3);
        ctrlc::set_handler(move || cancel_send.send(()).unwrap())
            .expect("could not set interrupt handler");

        let mut last_time = std::time::Instant::now();
        let mut last_bytes = 0;
        let mut last_dec_bytes = 0;
        let mut line_number = 0;
        loop {
            match cancel_recv.try_recv() {
                Ok(()) => {
                    debug!("received interrupt signal");
                    break;
                }
                Err(crossbeam::channel::TryRecvError::Empty) => (),
                Err(e) => panic!("unexpected error {}", e),
            }

            let line_offset = lines.bytes_read;
            line_number += 1;
            let line = match lines.next() {
                Ok(line) => line,
                Err(input::LineIterError::Eof) => break,
                Err(e) => {
                    error!("line iterator error: {}", e);
                    exit(-1);
                }
            };

            let sink = send.clone();
            let classes2 = Arc::clone(&classes);
            rayon_core::spawn(
                move || match wiki_data_line::handle_line(&line, &classes2, &sink) {
                    Ok(()) => (),
                    Err(e) => error!(
                        "error handling line {} at offset {}:{}\n\n",
                        line_number, line_offset, e
                    ),
                },
            );

            let elapsed = last_time.elapsed();
            if elapsed.as_secs() > 10 {
                let bytes_read =
                    (lines.input.bytes_read() - last_bytes) as f64 / elapsed.as_secs_f64();
                let dec_bytes_read =
                    (lines.bytes_read - last_dec_bytes) as f64 / elapsed.as_secs_f64();
                let total_bytes = lines.input.content_length().unwrap_or(0);
                let percent_complete = lines.input.bytes_read() as f64 / total_bytes as f64;
                let mut eta = (total_bytes - lines.input.bytes_read()) as f64 / bytes_read / 60.;
                let mut eta_unit = "m";
                if eta > 60. {
                    eta /= 60.;
                    eta_unit = "h";

                    if eta > 24. {
                        eta /= 24.;
                        eta_unit = "d 😔";
                    }
                }

                eprintln!(
                    "{:02.2}% (ETA: {:.1}{}) | {:.2} MB of {:.2} MB at {:.2} MB/s ({:.2} MB/s data)",
                    percent_complete * 100.,
                    eta,
                    eta_unit,
                    lines.input.bytes_read() as f64 / 1000_000.,
                    total_bytes as f64 / 1000_000.,
                    bytes_read / 1000_000.,
                    dec_bytes_read / 1000_000.,
                );
                last_bytes = lines.input.bytes_read();
                last_dec_bytes = lines.bytes_read;
                last_time = std::time::Instant::now();
            }
        }

        db_writer
    };

    debug!("Waiting for DB writer to join");
    db_writer.join().unwrap();
    debug!("Done!");
}
