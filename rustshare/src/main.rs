use actix_web::{web, App, HttpResponse, HttpServer, Error, Result, middleware::Logger};
use actix_multipart::Multipart;
use futures_util::{StreamExt, TryStreamExt};
use std::fs::{self, File};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce
};
use sha2::Sha256;
use pbkdf2::pbkdf2;
use hmac::Hmac;
use rand::Rng;
use tera::{Tera, Context};
use chrono::{DateTime, Utc};
use std::env;

// Define the encryption key size (32 bytes for AES-256)
const KEY_SIZE: usize = 32;
// Define the nonce size (12 bytes for AES-GCM)
const NONCE_SIZE: usize = 12;
// Define the salt size (16 bytes)
const SALT_SIZE: usize = 16;
// Define PBKDF2 iterations
const PBKDF2_ITERATIONS: u32 = 100_000;
// Maximum file size (10MB)
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct UploadResponse {
    file_id: String,
    message: String,
    expires_at: String,
}

#[derive(Serialize, Deserialize)]
struct DownloadRequest {
    password: String,
}

#[derive(Serialize, Deserialize)]
struct FileInfo {
    id: String,
    uploaded_at: String,
    size: usize,
}

// Derive key from password using PBKDF2
fn derive_key(password: &str, salt: &[u8]) -> [u8; KEY_SIZE] {
    let mut key = [0u8; KEY_SIZE];
    let _ = pbkdf2::<Hmac<Sha256>>(password.as_bytes(), salt, PBKDF2_ITERATIONS, &mut key);
    key
}

// Encrypt data with AES-256-GCM
fn encrypt_data(data: &[u8], password: &str) -> Result<Vec<u8>, String> {
    // Generate random salt and nonce
    let mut rng = rand::thread_rng();
    let mut salt = [0u8; SALT_SIZE];
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rng.fill(&mut salt);
    rng.fill(&mut nonce_bytes);
    
    // Derive key from password
    let key = derive_key(password, &salt);
    
    // Create cipher instance
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| format!("Failed to create cipher: {}", e))?;
    
    // Create nonce
    let nonce = Nonce::from_slice(&nonce_bytes);
    
    // Encrypt the data
    let encrypted_data = cipher.encrypt(nonce, data)
        .map_err(|e| format!("Encryption failed: {}", e))?;
    
    // Combine salt, nonce and encrypted data
    let mut result = Vec::with_capacity(SALT_SIZE + NONCE_SIZE + encrypted_data.len());
    result.extend_from_slice(&salt);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&encrypted_data);
    
    Ok(result)
}

// Decrypt data with AES-256-GCM
fn decrypt_data(data: &[u8], password: &str) -> Result<Vec<u8>, String> {
    if data.len() < SALT_SIZE + NONCE_SIZE {
        return Err("Data too short to contain salt and nonce".to_string());
    }
    
    // Extract salt, nonce and encrypted data
    let salt = &data[0..SALT_SIZE];
    let nonce_bytes = &data[SALT_SIZE..SALT_SIZE + NONCE_SIZE];
    let encrypted_data = &data[SALT_SIZE + NONCE_SIZE..];
    
    // Derive key from password
    let key = derive_key(password, salt);
    
    // Create cipher instance
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| format!("Failed to create cipher: {}", e))?;
    
    // Create nonce
    let nonce = Nonce::from_slice(nonce_bytes);
    
    // Decrypt the data
    let decrypted_data = cipher.decrypt(nonce, encrypted_data)
        .map_err(|e| format!("Decryption failed: {}", e))?;
    
    Ok(decrypted_data)
}

// Serve the main page
async fn index(tmpl: web::Data<Tera>) -> Result<HttpResponse, Error> {
    let mut ctx = Context::new();
    ctx.insert("title", "Secure File Sharing System");
    
    let body = tmpl.render("index.html", &ctx)
        .map_err(|_| actix_web::error::ErrorInternalServerError("Template error"))?;
    
    Ok(HttpResponse::Ok().content_type("text/html").body(body))
}

// Handle file upload
async fn upload_file(mut payload: Multipart) -> Result<HttpResponse, Error> {
    // Create uploads directory if it doesn't exist
    fs::create_dir_all("./uploads").map_err(|e| {
        actix_web::error::ErrorInternalServerError(format!("Failed to create directory: {}", e))
    })?;
    
    let mut file_data = Vec::new();
    let mut password = None;
    let mut file_size = 0;
    
    // Iterate over multipart stream
    while let Ok(Some(mut field)) = payload.try_next().await {
        let content_disposition = field.content_disposition();
        
        if let Some(name) = content_disposition.get_name() {
            match name {
                "file" => {
                    while let Some(chunk) = field.next().await {
                        let data = chunk.map_err(|e| {
                            actix_web::error::ErrorInternalServerError(format!("File read error: {}", e))
                        })?;
                        
                        // Check file size limit
                        file_size += data.len();
                        if file_size > MAX_FILE_SIZE {
                            return Ok(HttpResponse::BadRequest().body("File size exceeds 10MB limit"));
                        }
                        
                        file_data.extend_from_slice(&data);
                    }
                }
                "password" => {
                    let mut pwd = String::new();
                    while let Some(chunk) = field.next().await {
                        let data = chunk.map_err(|e| {
                            actix_web::error::ErrorInternalServerError(format!("Password read error: {}", e))
                        })?;
                        pwd.push_str(&String::from_utf8_lossy(&data));
                    }
                    password = Some(pwd);
                }
                _ => {}
            }
        }
    }
    
    // Check if we have both file and password
    if file_data.is_empty() {
        return Ok(HttpResponse::BadRequest().body("No file provided"));
    }
    
    let password = password.ok_or_else(|| {
        actix_web::error::ErrorBadRequest("No password provided")
    })?;
    
    // Encrypt the file data
    let encrypted_data = encrypt_data(&file_data, &password)
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("Encryption failed: {}", e)))?;
    
    // Generate a unique file ID
    let file_id = Uuid::new_v4().to_string();
    let filepath = format!("./uploads/{}", file_id);
    
    // Save the encrypted file
    let mut file = File::create(&filepath).map_err(|e| {
        actix_web::error::ErrorInternalServerError(format!("Failed to create file: {}", e))
    })?;
    
    file.write_all(&encrypted_data).map_err(|e| {
        actix_web::error::ErrorInternalServerError(format!("Failed to write file: {}", e))
    })?;
    
    // Calculate expiration time (24 hours from now)
    let now = SystemTime::now();
    let expires_at = now + std::time::Duration::from_secs(24 * 60 * 60);
    let expires_datetime: DateTime<Utc> = expires_at.into();
    
    // Return the file ID
    let response = UploadResponse {
        file_id: file_id.clone(),
        message: "File uploaded successfully".to_string(),
        expires_at: expires_datetime.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    };
    
    Ok(HttpResponse::Ok().json(response))
}

// Handle file download
async fn download_file(
    file_id: web::Path<String>,
    info: web::Json<DownloadRequest>,
) -> Result<HttpResponse, Error> {
    let password = &info.password;
    
    let filepath = format!("./uploads/{}", file_id);
    
    // Read the encrypted file
    let encrypted_data = fs::read(&filepath).map_err(|e| {
        actix_web::error::ErrorNotFound(format!("File not found: {}", e))
    })?;
    
    // Decrypt the data
    let decrypted_data = decrypt_data(&encrypted_data, password)
        .map_err(|e| actix_web::error::ErrorInternalServerError(format!("Decryption failed: {}", e)))?;
    
    // Return the decrypted file
    Ok(HttpResponse::Ok()
        .content_type("application/octet-stream")
        .append_header(("Content-Disposition", format!("attachment; filename=\"{}\"", file_id)))
        .body(decrypted_data))
}

// List all files (for admin purposes, would be protected in production)
async fn list_files() -> Result<HttpResponse, Error> {
    let mut files = Vec::new();
    
    for entry in fs::read_dir("./uploads").map_err(|e| {
        actix_web::error::ErrorInternalServerError(format!("Failed to read uploads directory: {}", e))
    })? {
        let entry = entry.map_err(|e| {
            actix_web::error::ErrorInternalServerError(format!("Failed to read directory entry: {}", e))
        })?;
        
        let metadata = entry.metadata().map_err(|e| {
            actix_web::error::ErrorInternalServerError(format!("Failed to get file metadata: {}", e))
        })?;
        
        let modified_time = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let datetime: DateTime<Utc> = modified_time.into();
        
        files.push(FileInfo {
            id: entry.file_name().to_string_lossy().into_owned(),
            uploaded_at: datetime.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            size: metadata.len() as usize,
        });
    }
    
    Ok(HttpResponse::Ok().json(files))
}

// Health check endpoint
async fn health_check() -> Result<HttpResponse, Error> {
    Ok(HttpResponse::Ok().body("Server is running"))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Initialize logger
    std::env::set_var("RUST_LOG", "actix_web=info");
    env_logger::init();
    
    
    // Create uploads directory if it doesn't exist
    fs::create_dir_all("./uploads").unwrap();
    fs::create_dir_all("./templates").unwrap();
    
    // Get port from environment variable (for deployment)
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .unwrap_or(8080);
    let server_url = format!("0.0.0.0:{}", port);
    println!("Starting server at http://{}", server_url);
    // Initialize Tera templates
    let mut tera = Tera::new("templates/**/*").unwrap();
    
    // If we're running in a production environment, we might need to load the template differently
    // For now, we'll just use the file system
    //println!("Starting server at http://0.0.0.0:{}", port);
    
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(web::Data::new(tera.clone()))
            .route("/", web::get().to(index))
            .route("/upload", web::post().to(upload_file))
            .route("/download/{file_id}", web::post().to(download_file))
            .route("/files", web::get().to(list_files))
            .route("/health", web::get().to(health_check))
    })
    .bind(server_url)?
    .run()
    .await
}