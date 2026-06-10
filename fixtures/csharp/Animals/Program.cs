using System.Collections.Generic;

namespace Animals;

public abstract class Animal
{
    public string Name { get; }

    protected Animal(string name)
    {
        Name = name;
    }

    public abstract string Sound();
}

public class Dog : Animal
{
    public Dog(string name) : base(name) { }

    public override string Sound() => "Woof";
}

public class Cat : Animal
{
    public Cat(string name) : base(name) { }

    public override string Sound() => "Meow";
}

public class Zoo
{
    private readonly List<Animal> _animals = new();

    public void Add(Animal animal) => _animals.Add(animal);

    public void AnnounceAll()
    {
        foreach (var a in _animals)
            Console.WriteLine($"{a.Name} says {a.Sound()}");
    }
}

class Program
{
    static void Main()
    {
        var zoo = new Zoo();
        zoo.Add(new Dog("Rex"));
        zoo.Add(new Cat("Whiskers"));
        zoo.AnnounceAll();
    }
}
