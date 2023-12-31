#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![allow(dead_code)]
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};

use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config, IpListenEndpoint, Stack, StackResources,
};
use heapless::Vec;

use esp_wifi::initialize;
use esp_wifi::wifi::{WifiStaDevice, WifiController, WifiState, WifiEvent, WifiDevice};
use esp_wifi::{EspWifiInitFor};
use static_cell::make_static;

use embassy_executor::Spawner;
use embassy_time::{ Duration, Timer};
use embassy_sync::{
    channel::{Channel},
    blocking_mutex::raw::CriticalSectionRawMutex
};

use esp_backtrace as _;
use esp32c3_hal::{
    gpio::{GpioPin, PushPull, Output},
    clock::ClockControl,
    embassy,
    Rng,
    IO,
    peripherals::{Peripherals},
    prelude::*
};

use picoserve::{
    routing::{get},
};


const WEB_TASK_POOL_SIZE : usize = 2;
static CHANNEL: Channel<CriticalSectionRawMutex, ControlMessages, 10> = Channel::new();

#[derive(Debug)]
enum ControlMessages{
    OnLight,
    OffLight,
    ReadTempAndHumidity,
}

const SSID: &str = "COPOLAND-PLUS";
const PASSWORD: &str = "Nhy6bgt5vfr4.";


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

    let mut led = io.pins.gpio12.into_push_pull_output();

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

    let seed = 1234; // very random, very secure seed

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
			log::info!("pause solicitado");
			sender.send(ControlMessages::OffLight).await;
                    },
                ),
            )
	    .route(
                ("/humidity-and-temp",),
                get(
                    || async move {
			let sender = CHANNEL.sender();
			sender.send(ControlMessages::ReadTempAndHumidity).await;
                    },
                ),
            )
    }
    
    let web_app = make_static!(make_app());

    let webserver_config = make_static!(picoserve::Config {
        start_read_request_timeout: Some(Duration::from_secs(15)),
        read_request_timeout: Some(Duration::from_secs(10)),
    });

    if let Err(why) = spawner.spawn(connection(controller)) {
	log::error!("Failed spawning 'connection' task: {why:?}");
    }
    
    if let Err(why) = spawner.spawn(net_task(&stack)) {
	log::error!("Failed spawning 'net_task' task: {why:?}");
    }
    
    for id in 0..WEB_TASK_POOL_SIZE {

	if let Err(why) = spawner.spawn(web_task(&stack, web_app, webserver_config)){
	    log::error!("Failed spawning 'web_task' ID: {id} task: {why:?}");
	}
    }

    let _ = led.toggle();


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
async fn connection(mut controller: WifiController<'static>) {
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
		controller.wait_for_event(WifiEvent::StaDisconnected).await;
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
