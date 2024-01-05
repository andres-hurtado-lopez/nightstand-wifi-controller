#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![allow(dead_code)]
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};

use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config, IpListenEndpoint, Stack, StackResources,
};

use esp_wifi::initialize;
use esp_wifi::wifi::{WifiStaDevice, WifiController, WifiState, WifiEvent, WifiDevice};
use esp_wifi::{EspWifiInitFor};
use static_cell::make_static;
use serde::Serialize;

use embassy_executor::Spawner;
use embassy_time::{ Duration, Timer};
use embassy_sync::{
    channel::{Channel},
    signal::Signal,
    blocking_mutex::raw::CriticalSectionRawMutex
};

use esp_backtrace as _;
use esp32c3_hal::{
    gpio::{GpioPin, PushPull, Output},
    i2c::I2C,
    clock::ClockControl,
    embassy,
    Rng,
    IO,
    peripherals::{Peripherals, I2C0},
    prelude::*
};

use embassy_futures::yield_now;

use picoserve::{
    routing::{get},
};


const WEB_TASK_POOL_SIZE : usize = 2;
static CHANNEL: Channel<CriticalSectionRawMutex, ControlMessages, 10> = Channel::new();
static SENSOR_SIGNAL: Signal<CriticalSectionRawMutex, SensorData> = Signal::new();

#[derive(Debug)]
enum ControlMessages{
    OnLight,
    OffLight,
    ReadTempAndHumidity,
}

#[derive(Debug, Serialize)]
struct SensorData{
    temperature: i32,
    pressure: i32,
}


const SSID: &str = "TP-LINK_E1E082";
const PASSWORD: &str = "11180647";

//Y29uc3QgU1NJRDogJnN0ciA9ICJDT1BPTEFORC1QTFVTIjs=
//Y29uc3QgUEFTU1dPUkQ6ICZzdHIgPSAiTmh5NmJndDV2ZnI0LiI7


#[main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("Nightstand WIFI Controller");

    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM.split();

    let clocks = ClockControl::max(system.clock_control).freeze();
    let timer_group0 = esp32c3_hal::timer::TimerGroup::new(peripherals.TIMG0, &clocks);

    
    embassy::init(
        &clocks,
        timer_group0.timer0,
    );

    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);

    let light = io.pins.gpio0.into_push_pull_output();
    let led_wifi_connection = io.pins.gpio12.into_push_pull_output();
    let i2c = peripherals.I2C0;
    let sda = io.pins.gpio4;
    let scl = io.pins.gpio5;
    let frecuency = 100u32.kHz();

    let i2c = I2C::new(
        i2c,
        sda,
        scl,
        frecuency,
        &clocks,
    );


    esp32c3_hal::interrupt::enable(
        esp32c3_hal::peripherals::Interrupt::GPIO,
        esp32c3_hal::interrupt::Priority::Priority1,
    ).unwrap();

    let timer = esp32c3_hal::systimer::SystemTimer::new(peripherals.SYSTIMER).alarm0;

    let wifi_init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    ).unwrap();
    
    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&wifi_init, wifi, WifiStaDevice).unwrap();
    
    let config = Config::dhcpv4(Default::default());

    let seed = 0xd95a_a7e0_1c31_9ba6; 

    // Init network stack
    let stack = &*make_static!(Stack::new(
        wifi_interface,
        config,
        make_static!(StackResources::<{WEB_TASK_POOL_SIZE + 1}>::new()),
        seed
    ));


    fn make_app() -> picoserve::Router<AppRouter,()> {
        picoserve::Router::new()
	    .route(
		"/",
		get(|| picoserve::response::File::html(include_str!("index.html")))
	    )
            .route(
                ("/on",),
                get(
                    || async move {
			let sender = CHANNEL.sender();
			sender.send(ControlMessages::OnLight).await;
                    },
                ),
            )
	    .route(
                ("/off",),
                get(
                    || async move {
			let sender = CHANNEL.sender();
			sender.send(ControlMessages::OffLight).await;
                    },
                ),
            )
	    .route(
                ("/humidity-and-temp",),
                get(
                    || async move {
			let reading_data = SENSOR_SIGNAL.wait().await;

			picoserve::response::json::Json(reading_data)
                    },
                ),
            )
    }
    
    let web_app = make_static!(make_app());

    let webserver_config = make_static!(picoserve::Config {
        start_read_request_timeout: Some(Duration::from_secs(15)),
        read_request_timeout: Some(Duration::from_secs(10)),
    });

    if let Err(why) = spawner.spawn(connection(controller, led_wifi_connection)) {
	log::error!("Failed spawning 'connection' task: {why:?}");
    }
    
    if let Err(why) = spawner.spawn(net_task(&stack)) {
	log::error!("Failed spawning 'net_task' task: {why:?}");
    }

    if let Err(why) = spawner.spawn(gpio_task(light)) {
	log::error!("Failed spawning 'net_task' task: {why:?}");
    }

    if let Err(why) = spawner.spawn(sensor_task(i2c)) {
	log::error!("Failed spawning 'net_task' task: {why:?}");
    }
    
    for id in 0..WEB_TASK_POOL_SIZE {

	if let Err(why) = spawner.spawn(web_task(&stack, web_app, webserver_config)){
	    log::error!("Failed spawning 'web_task' ID: {id} task: {why:?}");
	}
    }


}

struct EmbassyTimer;

impl picoserve::Timer for EmbassyTimer {
    type Duration = embassy_time::Duration;
    type TimeoutError = embassy_time::TimeoutError;

    async fn run_with_timeout<F: core::future::Future>(
        &mut self,
        duration: Self::Duration,
        future: F,
    ) -> Result<F::Output, Self::TimeoutError> {
        embassy_time::with_timeout(duration, future).await
    }
}

#[derive(Clone, Copy)]
struct SharedControl;
struct ParseSharedControl;

use core::str::FromStr;
impl FromStr for SharedControl {
    type Err = ParseSharedControl;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
	log::info!("shared state generation {s}");
	Ok(SharedControl)
    }
}

struct AppState {
    shared_control: SharedControl,
}

impl picoserve::extract::FromRef<AppState> for SharedControl {
    fn from_ref(state: &AppState) -> Self {
        state.shared_control
    }
}

type AppRouter = impl picoserve::routing::PathRouter<()>;


#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
    app: &'static picoserve::Router<AppRouter>,
    config: &'static picoserve::Config<Duration>,
){
    let mut rx_buffer = [0; 1536];
    let mut tx_buffer = [0; 1536];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(embassy_time::Duration::from_secs(1)));

    
    loop {
        log::info!("Waiting for incomming HTTP connection...");
        let r = socket
            .accept(IpListenEndpoint {
                addr: None,
                port: 80,
            })
            .await;
        log::info!("HTTP browser connected...");

        if let Err(e) = r {
            log::error!("connect error: {:?}", e);
            continue;
        }

	let (socket_rx, socket_tx) = socket.split();

        match picoserve::serve(
            app,
            EmbassyTimer,
            config,
            &mut [0; 2048],
            socket_rx,
            socket_tx,
        )
        .await
        {
            Ok(handled_requests_count) => {
                log::info!(
                    "{handled_requests_count} requests handled from {:?}",
                    socket.remote_endpoint()
                );
            }
            Err(err) => log::error!("Picoserver error: {err:?}"),
        }
	
        socket.close();
        socket.abort();
    }
}

#[embassy_executor::task]
async fn sensor_task(
    mut i2c: I2C<'static, I2C0>
){

    let sensor_address = bmp280_rs::i2c_address::I2CAddress::SdoGrounded;
    let sensor_config  = bmp280_rs::config::Config::weather_monitoring();

    let mut pressure = 0i32;
    let mut temperature = 0i32;

    
    match  bmp280_rs::BMP280::new(
	&mut i2c,
	sensor_address,
	sensor_config
    ) {
	Ok(mut sensor) => {
	    loop{

		if let Ok(t) = sensor.read_temperature(&mut i2c){
		    temperature = t;
		}

		if let Ok(p) = sensor.read_pressure(&mut i2c) {
		    pressure = p;
		}

		let sensor_data = SensorData{temperature, pressure};

		SENSOR_SIGNAL.signal(sensor_data);

		yield_now().await;
		
	    }
	},
	Err(why) => {
	    log::error!("Failed initializing bmp280 sensor: {why}");
	    loop {
		SENSOR_SIGNAL.signal(SensorData{temperature:-1, pressure:-1});
		yield_now().await;
	    }
	}
    }

}


#[embassy_executor::task]
async fn connection(
    mut controller: WifiController<'static>,
    mut led_wifi_connection: GpioPin<Output<PushPull>, 12>,
	
) {
    log::info!("start connection task");
    log::info!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::ApStarted => {
                // wait until we're no longer connected
		log::info!("WifiState::ApStarted waiting for ap to stop");
            },
	    WifiState::StaStarted => {
		log::info!("WifiState::StaStarted");
	    },
	    WifiState::StaConnected => {
		log::info!("WifiState::StaConnected");
		let _ = led_wifi_connection.set_high();
		controller.wait_for_event(WifiEvent::StaDisconnected).await;
		let _ = led_wifi_connection.set_low();
                Timer::after(Duration::from_millis(100)).await

	    },
	    WifiState::StaDisconnected => {
		log::info!("WifiState::StaDisconnected");
	    },
	    WifiState::StaStopped => {
		log::info!("WifiState::StaStopped");
	    },
	    WifiState::ApStopped => {
		log::info!("WifiState::ApStopped");
	    },
	    WifiState::Invalid => {
		log::info!("WifiState::Invalid");

		if !matches!(controller.is_started(), Ok(true)) {
		    let client_config = Configuration::Client(ClientConfiguration {
			ssid: SSID.try_into().unwrap(),
			password: PASSWORD.try_into().unwrap(),
			..Default::default()
		    });
		    controller.set_configuration(&client_config).unwrap();
		    log::info!("Starting wifi");
		    controller.start().await.unwrap();
		    log::info!("Wifi started!");
		}

		log::info!("About to connect to AP {SSID}...");
		match controller.connect().await {
		    Ok(_) => log::info!("Wifi connected!"),
		    Err(e) => {
			log::info!("Failed to connect to wifi: {e:?}");
			Timer::after(Duration::from_millis(5000)).await
		    }
		}

	    },


        }
	log::info!("connection task loop...!");
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    log::info!("net_task before");
    stack.run().await;
    log::info!("net_task after");
}

#[embassy_executor::task]
async fn gpio_task(
    mut light_gpio: GpioPin<Output<PushPull>, 0>,

){

    let receiver = CHANNEL.receiver();

    loop{

	match receiver.receive().await {
	    ControlMessages::OnLight => {
		let r = light_gpio.set_high();
		log::info!("light on: {:?}",r);
	    },
	    ControlMessages::OffLight => {
		let r = light_gpio.set_low();
		log::info!("light off: {:?}",r);
	    },
	    ControlMessages::ReadTempAndHumidity => {
	    },
	}
    }
    
}
