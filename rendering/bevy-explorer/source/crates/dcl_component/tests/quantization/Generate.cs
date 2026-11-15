using System;
using Decentraland.Networking.Bitwise;

class Generate
{
    static string Bits(float value) => BitConverter.SingleToUInt32Bits(value).ToString("x8");
    static void Linear(float value, float min, float max, int bits) =>
        Console.WriteLine($"linear\t{Bits(value)}\t{Bits(min)}\t{Bits(max)}\t00000000\t{bits}\t{Quantize.Encode(value, min, max, bits)}");
    static void Power(float value) =>
        Console.WriteLine($"power\t{Bits(value)}\t00000000\t{Bits(50f)}\t{Bits(2f)}\t8\t{Quantize.EncodePower(value, 50f, 2f, 8)}");

    static void Main()
    {
        foreach (var (min, max, bits) in new[] {
            (0f, 16f, 8), (0f, 200f, 13), (0f, 360f, 7),
            (0f, 3f, 5), (0f, 1f, 4), (-3000f, 3000f, 17)
        })
        {
            foreach (var value in new[] { min - 1f, min, MathF.BitIncrement(min),
                -0f, 0f, MathF.BitDecrement(max), max, max + 1f })
                Linear(value, min, max, bits);
            var steps = (1u << bits) - 1;
            foreach (var n in new[] { 0, 1, 2, 3, 4, 5, 6, 7, (int)steps - 2 })
            {
                var midpoint = min + (max - min) * ((n + 0.5f) / steps);
                Linear(MathF.BitDecrement(midpoint), min, max, bits);
                Linear(midpoint, min, max, bits);
                Linear(MathF.BitIncrement(midpoint), min, max, bits);
            }
        }
        foreach (var value in new[] { -51f, -50f, MathF.BitIncrement(-50f), -0f, 0f,
            float.Epsilon, -float.Epsilon, MathF.BitDecrement(50f), 50f, 51f })
            Power(value);
        foreach (var n in new[] { 0, 1, 2, 3, 4, 5, 6, 7, 125 })
        {
            var u = (n + 0.5f) / 127f;
            var midpoint = 50f * u * u;
            foreach (var sign in new[] { 1f, -1f })
            {
                Power(sign * MathF.BitDecrement(midpoint));
                Power(sign * midpoint);
                Power(sign * MathF.BitIncrement(midpoint));
            }
        }
    }
}
