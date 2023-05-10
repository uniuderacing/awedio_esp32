#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use awedio::{manager::Manager, manager::Renderer, Sound};
use std::ffi::c_void;
use std::ptr::null_mut;

/// An ESP32 backend for the I2S peripheral for ESP-IDF.
#[derive(Copy, Clone)]
pub struct Esp32Backend {
    /// The number of channels. 1 for mono, 2 for stereo...
    pub channel_count: u16,
    /// The number of samples per second.
    pub sample_rate: u32,
    /// The I2S peripheral port number to write samples to. Defaults to 0.
    pub i2s_port_num: u32,
    /// The size in frames of the samples buffer given to each call to I2S
    /// write.
    pub num_frames_per_write: usize,
    /// The stack size of the FreeRTOS task. Default may need to be increased if
    /// your Sounds sent to the renderer are complex.
    pub stack_size: u32,
    /// The priority of the FreeRTOS task.
    pub task_priority: u32,
    /// Whether the FreeRTOS task should be pinned to a core and if so what
    /// core.
    pinned_core_id: i32,
}

impl Esp32Backend {
    /// New backend with some defaults of:
    ///
    /// `i2s_port_num`: 0
    /// `num_frames_per_write`: 512
    /// `stack_size`: 30,000
    /// `task_priority`: 19
    /// `pinned_core_id`: tskNO_AFFINITY,
    ///
    /// Stack size can be substantially lower if not decoding MP3s. This should
    /// be improved in the future.
    pub fn with_defaults(channel_count: u16, sample_rate: u32) -> Self {
        Self {
            channel_count,
            sample_rate,
            i2s_port_num: 0,
            num_frames_per_write: 512,
            stack_size: 30000,
            task_priority: 19,
            pinned_core_id: esp_idf_sys::tskNO_AFFINITY as i32,
        }
    }
}

struct TaskArgs {
    backend: Esp32Backend,
    renderer: Renderer,
}

impl Esp32Backend {
    /// Start a new FreeRTOS task that will pull samples generated from Sounds
    /// sent to the returned Manager and write them to I2S.
    ///
    /// The task stops if the Manager and all of its clones are dropped.
    /// The user is responsible for setting up the I2S port before calling start
    /// (e.g. via esp_idf_sys::i2s_driver_install and esp_idf_sys::i2s_set_pin)
    pub fn start(&mut self) -> Manager {
        let (manager, mut renderer) = Manager::new();
        renderer.set_output_channel_count_and_sample_rate(self.channel_count, self.sample_rate);
        let awedio::NextSample::MetadataChanged = renderer.next_sample() else {
            panic!("MetadataChanged expected but not received.");
        };
        let args = Box::new(TaskArgs {
            backend: *self,
            renderer,
        });
        let res = unsafe {
            esp_idf_sys::xTaskCreatePinnedToCore(
                Some(audio_task),
                "TedioAudioBackend\0".as_bytes().as_ptr() as *const i8,
                self.stack_size,
                Box::into_raw(args) as *mut c_void,
                self.task_priority,
                null_mut(),
                self.pinned_core_id,
            )
        };

        // https://github.com/esp-rs/esp-idf-sys/issues/130
        // https://github.com/espressif/esp-idf/blob/60b90c5144267ce087287c099172fa5c8d374a54/components/freertos/include/freertos/projdefs.h#L51        const PD_PASS = 1;
        const PD_PASS: i32 = 1;
        if res != PD_PASS {
            panic!("Failed to start audio backend: {}", res);
        }
        manager
    }
}

extern "C" fn audio_task(arg: *mut c_void) {
    const SAMPLE_SIZE: usize = std::mem::size_of::<i16>();
    let mut task_args: Box<TaskArgs> = unsafe { Box::from_raw(arg as *mut TaskArgs) };
    let i2s_port_num = task_args.backend.i2s_port_num;
    let channel_count = task_args.backend.channel_count as usize;
    let sample_rate = task_args.backend.sample_rate;
    let num_frames_per_write = task_args.backend.num_frames_per_write;
    let buf_len = num_frames_per_write * channel_count * SAMPLE_SIZE;
    let mut buf = vec![0_u8; buf_len];
    let pause_time = std::time::Duration::from_micros(
        num_frames_per_write as u64 * 1_000_000 / sample_rate as u64,
    );
    let mut stopped = false;
    loop {
        task_args.renderer.on_start_of_batch();
        let mut paused = false;
        let mut finished = false;
        let mut have_data = true;
        for i in 0..(num_frames_per_write * channel_count) {
            let sample = match task_args.renderer.next_sample() {
                awedio::NextSample::Sample(s) => s,
                awedio::NextSample::MetadataChanged => {
                    unreachable!("we do not change the metadata of the renderer")
                }
                awedio::NextSample::Paused => {
                    paused = true;
                    if i == 0 {
                        have_data = false;
                        break;
                    }
                    0
                }
                awedio::NextSample::Finished => {
                    finished = true;
                    if i == 0 {
                        have_data = false;
                        break;
                    }
                    0
                }
            };

            let start_byte = i * SAMPLE_SIZE;
            buf[(start_byte)..(start_byte + SAMPLE_SIZE)]
                .copy_from_slice(sample.to_le_bytes().as_slice());
        }
        if have_data {
            if stopped {
                stopped = false;
                unsafe { esp_idf_sys::i2s_start(i2s_port_num) };
            }
            let mut bytes_written: usize = 0;
            let result = unsafe {
                esp_idf_sys::i2s_write(
                    i2s_port_num,
                    buf.as_ptr() as *const c_void,
                    buf.len().try_into().unwrap(),
                    &mut bytes_written as *mut _,
                    u32::MAX,
                )
            };
            assert!(result == esp_idf_sys::ESP_OK, "error writing i2s data");
            assert!(
                bytes_written as usize == buf.len(),
                "not all bytes written in i2s_write call"
            )
        }

        if finished {
            break;
        }
        if paused {
            if !stopped {
                stopped = true;
                unsafe { esp_idf_sys::i2s_stop(i2s_port_num) };
            }
            // TODO instead of sleeping and polling, have the Renderer
            // notify when a new sound is added and wait for that.
            std::thread::sleep(pause_time);
            continue;
        }
    }

    unsafe { esp_idf_sys::i2s_stop(i2s_port_num) };
    unsafe { esp_idf_sys::vTaskDelete(null_mut()) }
}