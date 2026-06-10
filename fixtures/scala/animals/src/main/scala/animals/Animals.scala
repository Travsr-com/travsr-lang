package animals

abstract class Animal(val name: String) {
  def sound(): String
}

class Dog(name: String) extends Animal(name) {
  override def sound(): String = "Woof"
}

class Cat(name: String) extends Animal(name) {
  override def sound(): String = "Meow"
}

class Zoo {
  private var animals: List[Animal] = List.empty

  def add(animal: Animal): Unit = {
    animals = animals :+ animal
  }

  def announceAll(): Unit = {
    animals.foreach(a => println(s"${a.name} says ${a.sound()}"))
  }
}

object Main extends App {
  val zoo = new Zoo()
  zoo.add(new Dog("Rex"))
  zoo.add(new Cat("Whiskers"))
  zoo.announceAll()
}
