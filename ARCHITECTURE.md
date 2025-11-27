# Architecture

## Overview

Maivin WebSrv is a web-based user interface server for monitoring and controlling the EdgeFirst Maivin platform.

## System Architecture

### Web Server

The WebSrv operates as an Actix-web based server with the following responsibilities:

- Serve static web UI assets
- Provide RESTful API endpoints
- Handle WebSocket connections for real-time updates
- Manage MCAP file playback
- System monitoring and diagnostics

### Key Components

1. **Web UI Layer**
   - Static file serving
   - Single-page application
   - Real-time visualization

2. **API Layer**
   - RESTful endpoints
   - WebSocket handlers
   - Authentication and authorization

3. **Data Layer**
   - MCAP file access
   - Zenoh integration
   - System information gathering

## Communication

### Zenoh Integration

The WebSrv uses Zenoh for:

- Real-time data streaming
- System status monitoring
- Control message publishing

### WebSocket Protocol

- Real-time data updates
- Bidirectional communication
- Event streaming

### Data Flow

```
Web Browser → WebSrv → Zenoh/ROS 2
     ↑          ↓          ↓
   UI      REST API    Maivin
  Updates   + WS      Platform
```

## Performance

### Optimization

- Static file caching
- Compressed asset delivery
- Efficient WebSocket handling
- Connection pooling

### Security

- HTTPS/TLS support (OpenSSL)
- Authentication mechanisms
- Input validation
- CORS configuration

## Configuration

Configuration is managed through command-line arguments and environment variables. See `args.rs` for available options.

## Future Enhancements

- Multi-user support
- Role-based access control
- Advanced data analytics
- Custom dashboards
- Remote deployment management
