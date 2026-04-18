# WASM + LLM Streaming Architecture Guide

# Introduction

This document describes in detail how WebAssembly (WASM) can be used to run Large Language Models (LLMs), with a focus on:

- Model streaming
- Memory mapping (mmap)
- Lazy loading
- KV cache management
- Paged Attention
- Browser vs server architectures
- Edge and serverless AI systems

This guide is intended for engineers working on:

- WASM runtimes
- AI infrastructure
- LLM inference systems
- Edge computing
- Distributed AI systems

---

# 1. Classical LLM Architecture

Traditional LLM stacks typically look like:

```
Python
 └── PyTorch / TensorFlow
      └── CUDA / GPU execution
           └── Model weights (multi-GB files)
```

### Challenges:

- Heavy runtime dependencies
- Slow startup time
- Large memory requirements
- Difficult deployment outside servers

### Example memory usage:

| Model | RAM Required |
|------|-------------|
| 7B model | ~4 GB |
| 13B model | ~8 GB |
| 70B model | 40GB+ |

---

# 2. WASM-Based LLM Architecture

WASM enables a portable execution environment:

```
WASM Runtime
 └── LLM runtime (.wasm)
      └── Model weights (.gguf / .bin)
```

Example execution:

```
wasmtime llm_runtime.wasm --model model.gguf
```

### Benefits:

- Portable across environments
- Secure sandboxing
- Fast startup
- Edge-friendly
- No heavy Python stack

---

# 3. LLM Internal Structure

A transformer model consists of sequential layers:

```
Input tokens
   ↓
Layer 1
   ↓
Layer 2
   ↓
Layer 3
   ↓
...
   ↓
Layer N
   ↓
Output tokens
```

Each layer includes:

- Attention weights
- Feed-forward weights
- Biases
- Normalization parameters

---

# 4. Full Model Loading (Baseline Approach)

Without optimization:

```
model.gguf (8GB)
   ↓
Fully loaded into RAM (8GB)
   ↓
Inference
```

### Problems:

- High memory usage
- Slow startup for large models

---

# 5. Model Streaming

Streaming avoids loading the entire model at once:

```
model.gguf (on disk)

RAM usage:
Layer 1 loaded
Layer 2 loaded
Layer 3 loaded
...
```

### Execution flow:

1. Load layer
2. Compute forward pass
3. Unload layer
4. Repeat for next layer

### Benefits:

- Lower RAM usage
- Enables larger models on limited hardware

### Trade-offs:

- Increased disk I/O
- Higher latency compared to full RAM loading

---

# 6. Memory Mapping (mmap)

Memory mapping allows models to be partially loaded:

```
model.gguf → mmap → virtual memory
```

### Characteristics:

- OS loads pages on demand
- Transparent paging mechanism
- Efficient disk-to-memory usage

### Advantages:

- Reduced RAM footprint
- Faster than manual streaming
- OS-level optimization

---

# 7. Lazy Loading

Lazy loading means loading model components only when needed:

```
Load layer → Compute → Release
Load next layer → Compute → Release
```

This reduces peak memory usage significantly.

---

# 8. KV Cache (Key-Value Cache)

The KV cache stores attention states for previous tokens.

### Formula:

```
KV cache size ∝ number of tokens × number of layers
```

### Example:

| Context size | Memory usage |
|-------------|-------------|
| 4K tokens | low |
| 32K tokens | high |
| 128K tokens | very high |

### Important:

Streaming models do NOT remove KV cache requirements.

---

# 9. KV Cache Externalization

The KV cache can be stored outside RAM:

```
LLM Runtime
   ↓
KV Cache
   ↓
External storage
```

### Possible backends:

- Redis
- RocksDB
- SQLite
- LMDB
- Distributed KV stores

### Pros:

- Scalable memory usage
- Persistent sessions
- Multi-instance sharing

### Cons:

- Higher latency
- Network overhead

---

# 10. Hybrid KV Cache Architecture

Recommended approach:

```
RAM (hot cache)
   ↓
Database (cold cache)
```

### Behavior:

- Recent tokens stored in RAM
- Older tokens offloaded to DB

### Benefits:

- Balanced performance
- Scalable memory usage

---

# 11. Paged Attention

Paged Attention is a memory management technique for KV cache.

Instead of storing KV cache as a continuous block, it is split into pages:

```
KV Cache:

Page 1 → tokens 1–128
Page 2 → tokens 129–256
Page 3 → tokens 257–384
```

### Key idea:

KV memory behaves like virtual memory paging in an operating system.

---

# 12. How Paged Attention Works

Instead of a contiguous KV cache:

```
[KV KV KV KV KV]
```

We use pages:

```
[Page][Page][Page][Page]
```

The model only accesses required pages during computation.

---

# 13. Memory Sharing

Paged Attention enables sharing between requests:

- Shared system prompts
- Shared prefix tokens

### Benefit:

- Reduced memory duplication
- Multi-user efficiency

---

# 14. Paged Attention + Streaming

Advanced architecture:

```
RAM (active pages)
   ↓
Disk (cold pages)
```

### Use cases:

- Long context windows
- Multi-session inference

---

# 15. Paged Attention + WASM

WASM-based architecture:

```
WASM runtime
   ↓
Paged KV cache
   ↓
External storage
```

### Benefits:

- Low memory footprint
- Portable execution
- Edge compatibility

---

# 16. Layer Execution Pipeline

During inference:

```
Load layer weights
Compute forward pass
Unload layer
```

All layers are executed sequentially.

---

# 17. Inference Pipeline

Full execution flow:

```
Input tokens
   ↓
Load layer
   ↓
Load KV cache
   ↓
Compute
   ↓
Unload layer
   ↓
Repeat
```

---

# 18. Modern LLM Architecture

Combined system:

```
WASM Runtime
   ↓
Model streaming
   ↓
Paged Attention
   ↓
KV cache storage
```

---

# 19. Browser vs Server Execution

## Server

- Full mmap support
- High performance
- Large models supported

## Browser

- No direct disk access
- Limited memory
- Requires streaming via HTTP or cache

---

# 20. Quantization

Model size reduction techniques:

| Format | Size reduction |
|--------|--------------|
| FP16 | baseline |
| Q8 | ~50% |
| Q4 | ~25% |
| Q2 | ~12% |

---

# 21. Advanced Modular Architectures

Models can be split into components:

```
Tokenizer.wasm
Embedding.wasm
LLM.wasm
Reranker.wasm
```

### Benefits:

- Plugin-based AI systems
- Dynamic model composition
- Multi-model inference

---

# 22. WASI-NN Acceleration

WASM can integrate with hardware acceleration:

```
WASM
 └── WASI-NN
      └── CPU / GPU / NPU
```

---

# 23. Use Cases

### Edge AI
- IoT devices
- CDN edge nodes

### Serverless AI
- Cold start optimization
- Lightweight inference

### AI Plugins
- Extensible systems
- Runtime model composition

---

# 24. Performance Comparison

| Mode | Memory | Speed |
|------|--------|------|
| Full RAM | High | Fast |
| Streaming | Medium | Medium |
| mmap | Medium | Fast |

---

# 25. Future Directions

Key trends:

- WASM-first AI runtimes
- Distributed inference systems
- Paged memory models
- Edge-native LLMs
- Agent-based architectures

---

# Conclusion

WASM combined with streaming, memory mapping, and paged attention enables:

- Efficient memory usage
- Portable AI execution
- Scalable inference systems
- Edge-compatible LLMs

This architecture is becoming a foundational pattern for modern AI infrastructure.

---

# End of Document

