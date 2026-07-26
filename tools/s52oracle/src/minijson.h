// Minimal JSON reader/writer for the s52oracle NDJSON protocol.
// Only what the protocol needs: objects, arrays, strings, numbers, true/false/null.
#pragma once

#include <cstdio>
#include <cstdlib>
#include <map>
#include <string>
#include <vector>

namespace mj {

struct Value;
using Object = std::vector<std::pair<std::string, Value>>;  // keeps insertion order
using Array = std::vector<Value>;

enum class Type { Null, Bool, Num, Str, Arr, Obj };

struct Value {
  Type type = Type::Null;
  bool b = false;
  double num = 0;
  bool num_is_int = false;
  std::string str;
  Array arr;
  Object obj;

  const Value *find(const std::string &key) const {
    if (type != Type::Obj) return nullptr;
    for (const auto &kv : obj)
      if (kv.first == key) return &kv.second;
    return nullptr;
  }
};

class Parser {
public:
  explicit Parser(const std::string &s) : s_(s) {}

  bool parse(Value &out) {
    ws();
    if (!value(out)) return false;
    ws();
    return true;
  }

private:
  const std::string &s_;
  size_t i_ = 0;

  void ws() {
    while (i_ < s_.size() && (s_[i_] == ' ' || s_[i_] == '\t' || s_[i_] == '\r' || s_[i_] == '\n'))
      ++i_;
  }
  bool lit(const char *l) {
    size_t n = strlen(l);
    if (s_.compare(i_, n, l) != 0) return false;
    i_ += n;
    return true;
  }

  bool value(Value &v) {
    if (i_ >= s_.size()) return false;
    char c = s_[i_];
    switch (c) {
      case '{': return object(v);
      case '[': return array(v);
      case '"': v.type = Type::Str; return string(v.str);
      case 't': v.type = Type::Bool; v.b = true; return lit("true");
      case 'f': v.type = Type::Bool; v.b = false; return lit("false");
      case 'n': v.type = Type::Null; return lit("null");
      default: return number(v);
    }
  }

  bool object(Value &v) {
    v.type = Type::Obj;
    ++i_;  // {
    ws();
    if (i_ < s_.size() && s_[i_] == '}') { ++i_; return true; }
    for (;;) {
      ws();
      std::string key;
      if (i_ >= s_.size() || s_[i_] != '"' || !string(key)) return false;
      ws();
      if (i_ >= s_.size() || s_[i_] != ':') return false;
      ++i_;
      ws();
      Value child;
      if (!value(child)) return false;
      v.obj.emplace_back(std::move(key), std::move(child));
      ws();
      if (i_ < s_.size() && s_[i_] == ',') { ++i_; continue; }
      if (i_ < s_.size() && s_[i_] == '}') { ++i_; return true; }
      return false;
    }
  }

  bool array(Value &v) {
    v.type = Type::Arr;
    ++i_;  // [
    ws();
    if (i_ < s_.size() && s_[i_] == ']') { ++i_; return true; }
    for (;;) {
      ws();
      Value child;
      if (!value(child)) return false;
      v.arr.push_back(std::move(child));
      ws();
      if (i_ < s_.size() && s_[i_] == ',') { ++i_; continue; }
      if (i_ < s_.size() && s_[i_] == ']') { ++i_; return true; }
      return false;
    }
  }

  bool string(std::string &out) {
    ++i_;  // opening quote
    out.clear();
    while (i_ < s_.size()) {
      char c = s_[i_++];
      if (c == '"') return true;
      if (c != '\\') { out.push_back(c); continue; }
      if (i_ >= s_.size()) return false;
      char e = s_[i_++];
      switch (e) {
        case '"': out.push_back('"'); break;
        case '\\': out.push_back('\\'); break;
        case '/': out.push_back('/'); break;
        case 'b': out.push_back('\b'); break;
        case 'f': out.push_back('\f'); break;
        case 'n': out.push_back('\n'); break;
        case 'r': out.push_back('\r'); break;
        case 't': out.push_back('\t'); break;
        case 'u': {
          if (i_ + 4 > s_.size()) return false;
          unsigned cp = (unsigned)strtoul(s_.substr(i_, 4).c_str(), nullptr, 16);
          i_ += 4;
          // Minimal UTF-8 encode (BMP only; surrogate pairs are passed through
          // as replacement, the protocol never carries them).
          if (cp < 0x80) {
            out.push_back((char)cp);
          } else if (cp < 0x800) {
            out.push_back((char)(0xC0 | (cp >> 6)));
            out.push_back((char)(0x80 | (cp & 0x3F)));
          } else {
            out.push_back((char)(0xE0 | (cp >> 12)));
            out.push_back((char)(0x80 | ((cp >> 6) & 0x3F)));
            out.push_back((char)(0x80 | (cp & 0x3F)));
          }
          break;
        }
        default: return false;
      }
    }
    return false;
  }

  bool number(Value &v) {
    size_t start = i_;
    if (i_ < s_.size() && (s_[i_] == '-' || s_[i_] == '+')) ++i_;
    bool isint = true;
    while (i_ < s_.size()) {
      char c = s_[i_];
      if (c >= '0' && c <= '9') { ++i_; continue; }
      if (c == '.' || c == 'e' || c == 'E' || c == '-' || c == '+') { isint = false; ++i_; continue; }
      break;
    }
    if (i_ == start) return false;
    v.type = Type::Num;
    v.num = atof(s_.substr(start, i_ - start).c_str());
    v.num_is_int = isint;
    return true;
  }
};

// ---- writing ----

inline void esc(std::string &out, const std::string &s) {
  out.push_back('"');
  for (unsigned char c : s) {
    switch (c) {
      case '"': out += "\\\""; break;
      case '\\': out += "\\\\"; break;
      case '\n': out += "\\n"; break;
      case '\r': out += "\\r"; break;
      case '\t': out += "\\t"; break;
      default:
        if (c < 0x20) {
          char buf[8];
          snprintf(buf, sizeof buf, "\\u%04x", c);
          out += buf;
        } else {
          out.push_back((char)c);
        }
    }
  }
  out.push_back('"');
}

inline std::string strlist(const std::vector<std::string> &v) {
  std::string out = "[";
  for (size_t i = 0; i < v.size(); ++i) {
    if (i) out += ",";
    esc(out, v[i]);
  }
  out += "]";
  return out;
}

}  // namespace mj
