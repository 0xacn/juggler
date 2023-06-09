use env_logger::Env;
use yew::prelude::*;

use hyper::body::HttpBody;
use hyper::client::connect::HttpConnector;
use hyper::header::HeaderValue;
use hyper::{Body, Client, Method, Request};

use log::{error, info};
use serde::Deserialize;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use structopt::StructOpt;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{channel, Receiver};

#[function_component]
fn App() -> Html {
    let counter = use_state(|| 0);
    let onclick = {
        let counter = counter.clone();
        move |_| {
            let value = *counter + 1;
            counter.set(value);
        }
    };

    html! {
        <div>
            <button {onclick}>{ "+1" }</button>
            <p>{ *counter }</p>
        </div>
    }
}

#[derive(StructOpt)]
#[structopt(name = "load_balancer")]
struct Opt {
    #[structopt(short = "p", long = "port", default_value = "8000")]
    port: u16,
    #[structopt(
        short = "s",
        long = "servers",
        default_value = "http://localhost:8081,http://localhost:8082"
    )]
    servers: Vec<String>,
    #[structopt(short = "w", long = "weights", default_value = "1,1")]
    weights: Vec<usize>,
}

#[derive(Debug, Deserialize)]
struct RequestData {
    message: String,
}

struct LoadBalancer {
    servers: Vec<String>,
    weights: Vec<usize>,
    current: Mutex<usize>,
    counter: Mutex<usize>,
}

impl LoadBalancer {
    fn new(servers: Vec<String>, weights: Vec<usize>) -> LoadBalancer {
        LoadBalancer {
            servers,
            weights,
            current: Mutex::new(0),
            counter: Mutex::new(0),
        }
    }

    fn get_server(&self) -> String {
        let mut current = self.current.lock().unwrap();
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;

        if *counter >= self.weights[*current] {
            *counter = 0;
            *current = (*current + 1) % self.servers.len();
        }
        let server = self.servers[*current].clone();
        server
    }
}

/* Proxy request */
async fn proxy_request(
    client: &Client<HttpConnector>,
    server: &str,
    request: Request<Body>,
) -> Result<String, String> {
    let mut proxy_request = request;
    *proxy_request.uri_mut() = server.parse().map_err(|e| format!("Invalid URI: {}", e))?;
    proxy_request
        .headers_mut()
        .insert("host", HeaderValue::from_str(server).unwrap());
    let response = client
        .request(proxy_request)
        .await
        .map_err(|e| format!("Error: {}", e))?;
    let response_body = response
        .into_body()
        .map_err(|e| format!("Failed to read response body: {:?}", e));
    let response_bytes = hyper::body::to_bytes(response_body)
        .await
        .map_err(|e| format!("Failed to read response bytes: {:?}", e))?;
    let response_str = String::from_utf8_lossy(&response_bytes).to_string();
    Ok(response_str)
}

async fn handle_request(
    load_balancer: Arc<LoadBalancer>,
    client: Client<HttpConnector>,
    mut stream: TcpStream,
) -> Result<(), String> {
    let mut buffer = [0; 1024];
    stream
        .read(&mut buffer)
        .map_err(|e| format!("Failed to read from stream: {}", e))?;
    let request_str = String::from_utf8_lossy(&buffer[..]).to_string();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/echo")
        .header("content-type", "application/json")
        .body(Body::from(request_str.clone()))
        .map_err(|e| format!("Failed to build request: {}", e))?;
    let server = load_balancer.get_server();
    let response_str = match proxy_request(&client, &server, request).await {
        Ok(response) => response,
        Err(e) => {
            error!("Error in proxy request: {}", e);
            return Err(format!("Error in proxy request: {}", e));
        }
    };

    let response_data: RequestData = serde_json::from_str(&response_str)
        .map_err(|e| format!("Failed to parse response: {}", e))?;
    let response_message = response_data.message;
    let response = format!("Response from server {}:\n", response_message);
    stream
        .write(response.as_bytes())
        .map_err(|e| format!("Failed to write to stream: {}", e))?;

    Ok(())
}

async fn accept_connection(
    load_balancer: Arc<LoadBalancer>,
    client: Client<HttpConnector>,
    mut receiver: Receiver<TcpStream>,
) {
    while let Some(stream) = receiver.recv().await {
        let load_balancer = load_balancer.clone();
        let client = client.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_request(load_balancer, client, stream).await {
                error!("{}", e);
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    //yew::Renderer::<App>::new().render();
    
    // Main logic, uses handle_request and runs load balancer
    let opt = Opt::from_args();
    env_logger::from_env(Env::default().default_filter_or("info")).init();
    let listener = TcpListener::bind(format!("localhost:{}", opt.port))
        .map_err(|e| format!("Failed to bind to port {}: {:?}", opt.port, e))?;

    let (sender, receiver) = channel::<TcpStream>(1024);
    let client = Client::new();

    let opt_servers = vec![
        "server1".to_string(),
        "server2".to_string(),
        "server3".to_string(),
    ];
    let servers = opt_servers.clone();
    for i in 0..num_cpus::get() {
        let receiver = tokio::sync::mpsc::channel(1024).1;
        let load_balancer = Arc::new(LoadBalancer::new(servers.clone(), vec![]));
        let client = client.clone();
        thread::spawn(move || {
            let rt = Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name(format!("worker-{}", i))
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(accept_connection(load_balancer, client, receiver));
        });

        info!("spawned worker thread {} ", i)
    }

    while let Ok(stream) = listener.accept() {
        sender
            .send(stream.0)
            .await
            .map_err(|e| format!("Failed to send connection to worker: {:?}", e))?;
    }

    Ok(())
}
