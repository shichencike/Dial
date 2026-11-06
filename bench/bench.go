// Dial 性能基准 —— Go (go build) 实现。
//
// 28 个跨语言等价的纯整数微基准。第一个命令行参数指定任务名，程序只在
// stdout 打印一行十进制校验和；计时由外部采集器负责。
// 一致性铁律见 bench.py 顶部注释。全程 int64，所有被取模/相除的值非负。
// 编译：go build -o bench_go bench.go
package main

import (
	"fmt"
	"os"
	"strings"
)

const (
	LOOP  = 1000000
	MID   = 100000
	ARR   = 2048
	SORTN = 512
	MOD   = int64(1000000007)
	MOD2  = int64(1000003)
)

var S = "abcdefghijklmnopqrstuvwxyz0123456789"

func b01() int64 {
	var s int64
	for i := int64(0); i < LOOP; i++ {
		s = (s + i) % MOD
	}
	return s
}
func b02() int64 {
	s := int64(1)
	for i := int64(0); i < MID; i++ {
		s = (s*31 + i) % MOD
	}
	return s
}
func b03() int64 {
	var s int64
	for i := int64(1); i < MID; i++ {
		s = (s + (i*1103515245)/7 + i%97) % MOD
	}
	return s
}
func b04() int64 {
	var s int64
	for i := int64(0); i < LOOP; i++ {
		s = (s*3 + 7) % MOD
	}
	return s
}
func b05() int64 {
	var s int64
	for a := int64(0); a < 20000; a++ {
		base, r := a%MOD2+1, int64(1)
		for j := 0; j < 8; j++ {
			r = (r * base) % MOD
		}
		s = (s + r) % MOD
	}
	return s
}
func gcdI(a, b int64) int64 {
	for b != 0 {
		a, b = b, a%b
	}
	return a
}
func b06() int64 {
	var s int64
	for i := int64(1); i < 30000; i++ {
		s = (s + gcdI(i*7+1, i*13+3)) % MOD
	}
	return s
}
func b07() int64 {
	a, b := int64(0), int64(1)
	for i := int64(0); i < LOOP; i++ {
		t := (a + b) % MOD
		a, b = b, t
	}
	return a
}
func b08() int64 {
	var s int64
	for n := int64(1); n < 20000; n++ {
		x, st := n, int64(0)
		for x > 1 {
			if x%2 == 0 {
				x = x / 2
			} else {
				x = 3*x + 1
			}
			st++
		}
		s = (s + st) % MOD
	}
	return s
}
func fibR(n int64) int64 {
	if n < 2 {
		return n
	}
	return fibR(n-1) + fibR(n-2)
}
func b09() int64 {
	var s int64
	for k := 0; k < 3; k++ {
		s = (s + fibR(23)) % MOD
	}
	return s
}
func rsumR(n int64) int64 {
	if n == 0 {
		return 0
	}
	return (n + rsumR(n-1)) % MOD
}
func b10() int64 {
	var s int64
	for k := 0; k < 50000; k++ {
		s = (s + rsumR(25)) % MOD
	}
	return s
}
func treeR(n int64) int64 {
	if n < 2 {
		return 1
	}
	return (treeR(n-1) + treeR(n-2) + 1) % MOD
}
func b11() int64 {
	var s int64
	for k := 0; k < 200; k++ {
		s = (s + treeR(18)) % MOD
	}
	return s
}
func evR(n int64) int64 {
	if n == 0 {
		return 1
	}
	return odR(n - 1)
}
func odR(n int64) int64 {
	if n == 0 {
		return 0
	}
	return evR(n - 1)
}
func b12() int64 {
	var s int64
	for k := int64(0); k < 20000; k++ {
		s = (s + evR(k%60+1)) % MOD
	}
	return s
}
func b13() int64 {
	var a [ARR]int64
	var s int64
	for i := 0; i < ARR; i++ {
		a[i] = (int64(i)*31 + 7) % MOD2
	}
	for j := 0; j < ARR; j++ {
		s = (s + a[j]) % MOD
	}
	return s
}
func b14() int64 {
	var a [ARR]int64
	var idx, s int64
	for i := 0; i < ARR; i++ {
		a[i] = (int64(i) * 17) % MOD2
	}
	for i := int64(0); i < LOOP; i++ {
		idx = (idx*1103515245 + 12345) & (ARR - 1)
		s = (s + a[idx]) % MOD
		a[idx] = (a[idx] + 1) % MOD2
	}
	return s
}
func b15() int64 {
	var a [SORTN]int64
	var s int64
	for i := 0; i < SORTN; i++ {
		a[i] = int64(SORTN - i)
	}
	for i := 0; i < SORTN; i++ {
		for j := 0; j < SORTN-1-i; j++ {
			if a[j] > a[j+1] {
				a[j], a[j+1] = a[j+1], a[j]
			}
		}
		s = (s + a[i]) % MOD
	}
	return s
}
func b16() int64 {
	var a [ARR]int64
	var s int64
	for i := 0; i < ARR; i++ {
		a[i] = (int64(i)*13 + 5) % MOD2
	}
	for r := 0; r < 100; r++ {
		for i := 0; i < ARR/2; i++ {
			a[i], a[ARR-1-i] = a[ARR-1-i], a[i]
		}
		for j := 0; j < ARR; j++ {
			s = (s + a[j]) % MOD
		}
	}
	return s
}
func b17() int64 {
	const n = 64
	var A, B, C [n * n]int64
	for i := 0; i < n*n; i++ {
		A[i] = (int64(i)*7 + 3) % MOD2
		B[i] = (int64(i)*11 + 5) % MOD2
	}
	for i := 0; i < n; i++ {
		for k := 0; k < n; k++ {
			aik := A[i*n+k]
			for j := 0; j < n; j++ {
				C[i*n+j] += aik * B[k*n+j]
			}
		}
	}
	var s int64
	for v := 0; v < n*n; v++ {
		s = (s + C[v]) % MOD
	}
	return s
}
func b18() int64 {
	var a [ARR]int64
	var s int64
	for r := 0; r < 200; r++ {
		for i := 0; i < ARR; i++ {
			a[i] = (int64(i)*13 + 5) % MOD2
		}
		for i := 1; i < ARR; i++ {
			a[i] = (a[i] + a[i-1]) % MOD
		}
		for j := 0; j < ARR; j++ {
			s = (s + a[j]) % MOD
		}
	}
	return s
}
func b19() int64 {
	var s int64
	L := len(S)
	for r := 0; r < 20000; r++ {
		for c := 0; c < L; c++ {
			s = (s + int64(S[c])) % MOD
		}
	}
	return s
}
func b20() int64 {
	x := "x"
	for i := 0; i < 4000; i++ {
		x = x + "y"
	}
	return (int64(len(x))*31 + int64(x[0]) + int64(x[len(x)-1])) % MOD
}
func b21() int64 {
	subs := [4]string{"mnop", "0123", "xyz", "abcd"}
	var s int64
	for k := int64(0); k < 20000; k++ {
		s = (s + int64(strings.Index(S, subs[k%4]))) % MOD
	}
	return s
}
func b22() int64 {
	base := S + "-" + S
	var s int64
	for k := int64(0); k < 10000; k++ {
		f := int(k % 20)
		for c := f; c < f+10; c++ {
			s = (s + int64(base[c])) % MOD
		}
	}
	return s
}
func b23() int64 {
	var s int64
	for i := int64(0); i < MID; i++ {
		x, c := i, int64(0)
		for x != 0 {
			c += x & 1
			x >>= 1
		}
		s = (s + c) % MOD
	}
	return s
}
func b24() int64 {
	x, s := int64(1), int64(0)
	for i := int64(0); i < LOOP; i++ {
		x ^= (x << 13) & 0xFFFFFFFF
		x ^= x >> 7
		x ^= (x << 17) & 0xFFFFFFFF
		x &= 0xFFFFFFFF
		s = (s + x%MOD2) % MOD
	}
	return s
}
func b25() int64 {
	var s int64
	for i := int64(0); i < LOOP; i++ {
		r := i % 3
		if r == 0 {
			s = (s + i) % MOD
		} else if r == 1 {
			s = (s + i*2) % MOD
		} else {
			s = (s + i*3) % MOD
		}
	}
	return s
}
func b26() int64 {
	var s int64
	for i := int64(0); i < 1000; i++ {
		for j := int64(0); j < 1000; j++ {
			s = (s + (i ^ j)) % MOD
		}
	}
	return s
}
func b27() int64 {
	var s int64
	for i := int64(0); i < LOOP; i++ {
		if ((i%7 == 0) && (i%3 != 0)) || (i&1 != 0) {
			s = (s + 1) % MOD
		} else {
			s = (s + 2) % MOD
		}
	}
	return s
}
func b28() int64 {
	var a [ARR]int64
	var s int64
	for i := 0; i < ARR; i++ {
		a[i] = (int64(i)*29 + 11) % MOD2
	}
	for i := int64(0); i < MID; i++ {
		p := i & (ARR - 1)
		a[p] = (a[p] + i) % MOD2
		s = (s*31 + a[p] + (i & 255)) % MOD
	}
	return s
}

var tasks = map[string]func() int64{
	"b01": b01, "b02": b02, "b03": b03, "b04": b04, "b05": b05, "b06": b06,
	"b07": b07, "b08": b08, "b09": b09, "b10": b10, "b11": b11, "b12": b12,
	"b13": b13, "b14": b14, "b15": b15, "b16": b16, "b17": b17, "b18": b18,
	"b19": b19, "b20": b20, "b21": b21, "b22": b22, "b23": b23, "b24": b24,
	"b25": b25, "b26": b26, "b27": b27, "b28": b28,
}

func main() {
	// 用法：bench_go <任务名> [重复次数]
	// 采集器用「单次进程内重复 N 次」自校准，摊薄进程启动开销；
	// 重复调用的是纯函数式任务，校验和恒定。
	name := "noop"
	if len(os.Args) > 1 {
		name = os.Args[1]
	}
	rep := int64(1)
	if len(os.Args) > 2 {
		fmt.Sscanf(os.Args[2], "%d", &rep)
	}
	fn, ok := tasks[name]
	var r int64
	for q := int64(0); q < rep; q++ {
		if ok {
			r = fn()
		} else {
			r = 0
		}
	}
	fmt.Println(r)
}
